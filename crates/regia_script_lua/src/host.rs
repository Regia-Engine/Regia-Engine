// SPDX-License-Identifier: MPL-2.0
//! The mod host: loading, the `regia` table, calls under budget with error isolation, events.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use mlua::{Function, Lua, MultiValue, Table, Value};
use regia_app::prelude::{Resource, bevy_ecs};
use regia_core::math::{Rng, Vec3};
use serde::{Deserialize, Serialize};

use crate::authority::{
    self, HostCommand, HostCommandRules, MAX_SNAPSHOT_BYTES, ModAuthority, ScriptAuthority,
};
use crate::components::{ComponentDecl, LuaValue, MAX_DECLS_PER_MOD, parse_decl};
use crate::error::{HostError, LuaErrorCode, ModError, classify};
use crate::files::{ModFiles, is_safe_relative};
use crate::manifest::{LUAMOD_FILE, LuaModManifest};
use crate::sandbox::{self, Budget, LuaLimits};
use crate::state::{self, LuaStateFile, ModState, SavedModEntity, StateCarry, StateTree};
use crate::value::{MAX_VALUE_BYTES, ScriptValue, lua_type, vec3_arg};
use crate::world::{JournalWorld, ModWorld};

mod mount;

/// Errors kept per mod.
pub const MAX_ERRORS_KEPT: usize = 32;
/// Longest log line, in bytes.
const MAX_LOG_LINE: usize = 1024;
/// Longest event name, in bytes.
const MAX_EVENT_NAME: usize = 128;
/// Registry key of the current call's world dispatcher.
const HOST_KEY: &str = "regia.host";
/// Registry key of the `regia` table (a script can overwrite the global).
const REGIA_KEY: &str = "regia.table";
/// Registry key of the event handler table (`name → { fn, … }`).
const HANDLERS_KEY: &str = "regia.handlers";
/// Registry key of the script-component declarations (`name → decl table with on_update`).
const COMPONENTS_KEY: &str = "regia.components";
/// Registry keys of `require`'s cache and in-progress set.
const LOADED_KEY: &str = "regia.loaded";
const LOADING_KEY: &str = "regia.loading";
/// The id of `lua.run`'s throwaway state.
const SCRATCH_ID: &str = "agent.scratch";
/// Handlers one mod may subscribe to one event.
pub const MAX_HANDLERS_PER_EVENT: usize = 256;
/// Longest chunk [`LuaMods::run_chunk`] accepts, in bytes.
pub const MAX_CHUNK_BYTES: usize = 64 * 1024;
/// Instructions a handler call costs before it runs: a delivery of many empty handlers is work.
const HANDLER_CALL_COST: u64 = 64;
/// Instructions one `regia.find` costs, plus one per this many entities it looks at.
const FIND_BASE_COST: u64 = 64;
const FIND_ENTITIES_PER_INSTRUCTION: u64 = 8;
/// Budget instructions one world operation costs (the scoped dispatcher and the host call),
/// ADR-1957 § 2.
const WORLD_OP_COST: u64 = 16;
/// Instructions one per-entity component update pass costs before it runs, plus one per this
/// many entities the world scans (like `FIND_BASE_COST`).
const COMPONENT_BASE_COST: u64 = 64;
const COMPONENT_ENTITIES_PER_INSTRUCTION: u64 = 8;

/// Most events held for the game to drain, and most bytes of them. The plugin does not drain them:
/// a game with no reader must not grow without end. The oldest go first.
pub const MAX_EMITTED_HELD: usize = 65_536;
/// See [`MAX_EMITTED_HELD`].
pub const MAX_EMITTED_HELD_BYTES: u64 = 64 * 1024 * 1024;

/// Per-state data the `regia` functions reach through `Lua::app_data_mut`.
pub(crate) struct ModRuntime {
    pub(crate) id: String,
    pub(crate) limits: LuaLimits,
    stream: u64,
    rng: Rng,
    log: VecDeque<String>,
    call_log: Vec<String>,
    outbox: Vec<EmittedEvent>,
    emits_this_call: u32,
    emit_bytes_this_call: u64,
    spawns_this_call: u32,
    /// The entity cap this mod runs under: the host's `max_entities_per_mod`, or the lower cap
    /// `LuaMods::set_entity_cap` gave it (ADR-2418).
    entity_cap: usize,
    /// Spawns the entity cap refused.
    entity_cap_refused: u64,
    /// Entities the cap does not count, sorted: during a load or a reload, the previous
    /// incarnation's entities, which go (or are taken over) before the mod starts.
    cap_excluded: Vec<u64>,
    /// The declarations this version made with `regia.component` (cached; a script cannot reach
    /// the registry table, so they cannot change after the entry chunk).
    decls: Vec<ComponentDecl>,
    budget: Arc<Budget>,
    /// The host or presentation side: the command queue and the snapshot (ADR-8466).
    pub(crate) authority: ModAuthority,
}

impl ModRuntime {
    /// The running call's budget.
    pub(crate) fn budget(&self) -> Arc<Budget> {
        Arc::clone(&self.budget)
    }
}

/// What every mod a host builds shares about its side of the game (ADR-8466).
#[derive(Debug, Clone, Default)]
pub(crate) struct HostSide {
    authority: ScriptAuthority,
    rules: Arc<HostCommandRules>,
    snapshot: Option<Arc<ScriptValue>>,
    generation: u64,
    weight: u64,
}

/// An event a mod sent to the game with `regia.emit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmittedEvent {
    /// The mod that sent it.
    pub mod_id: String,
    /// The event name.
    pub name: String,
    /// The payload.
    pub value: ScriptValue,
    /// The entity the mod aimed the event at (`regia.emit(name, value, target)`, spec 84 as amended
    /// by spec 102 § 4.4): its bits, or `None` when the mod named no target. A host routes it, for
    /// example into that entity's logic-graph inbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<u64>,
}

/// Whether a mod runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModStatus {
    /// Calls run.
    Active,
    /// An error stopped the mod; calls skip it until a restart.
    Disabled {
        /// The error's protocol code.
        code: LuaErrorCode,
        /// The error message.
        message: String,
    },
}

/// A mod's call counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModCounters {
    /// Calls made into the mod (load, hooks, handlers, agent chunks).
    pub calls: u64,
    /// Instructions of the last call (a multiple of the hook interval).
    pub last_instructions: u64,
    /// Most instructions of any call.
    pub peak_instructions: u64,
    /// Bytes the state holds now.
    pub memory_bytes: u64,
    /// Most bytes seen after a call.
    pub peak_memory_bytes: u64,
    /// Hot reloads that took effect.
    pub reloads: u64,
    /// Hot reloads that failed and rolled back.
    pub failed_reloads: u64,
}

/// One global (or table field) as `lua.globals` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlobalEntry {
    /// The key, as a string.
    pub key: String,
    /// The Lua type name.
    pub kind: String,
    /// The value, for scalars, vectors and entity handles.
    pub value: Option<ScriptValue>,
    /// The raw length, for tables.
    pub len: Option<usize>,
}

/// The outcome of [`LuaMods::run_chunk`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunOutcome {
    /// Whether the chunk ran to its end.
    pub ok: bool,
    /// What it returned (non-plain values are described as `"<type>"` strings).
    pub results: Vec<ScriptValue>,
    /// Why it failed.
    pub error: Option<ModError>,
    /// Instructions it used.
    pub instructions: u64,
    /// Bytes the state held afterwards.
    pub memory_bytes: u64,
    /// Lines it printed.
    pub log: Vec<String>,
}

/// How far a hot reload got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReloadStage {
    /// Reading and checking the new files and manifest.
    Files,
    /// Compiling every `.lua` file.
    Compile,
    /// The new version's entry chunk.
    Entry,
    /// The old version's `on_save` and the read of its state.
    Snapshot,
    /// Handing the old state to the new version (`on_migrate` included).
    State,
    /// The new version's `on_load` (or `on_start`).
    Start,
    /// The new version runs.
    Done,
}

impl ReloadStage {
    /// The name the agent methods use.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            ReloadStage::Files => "files",
            ReloadStage::Compile => "compile",
            ReloadStage::Snapshot => "snapshot",
            ReloadStage::Entry => "entry",
            ReloadStage::State => "state",
            ReloadStage::Start => "start",
            ReloadStage::Done => "done",
        }
    }
}

/// The outcome of a hot reload ([`LuaMods::reload`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReloadOutcome {
    /// The mod.
    pub id: String,
    /// Whether the new version runs. When `false` the old Lua state, handlers and entities stay, but
    /// the error ring (`lua.errors`) and the reload counters still record the failure.
    pub ok: bool,
    /// Where it stopped ([`ReloadStage::Done`] on success).
    pub stage: ReloadStage,
    /// What happened to `regia.state`. `None` on failure: the old version keeps its own.
    pub state: Option<StateCarry>,
    /// What the reload worked around: the old version's `on_save` failed (its state was read as
    /// it stood), or its state could not be read (the state was reset).
    pub note: Option<String>,
    /// Why it failed.
    pub error: Option<ModError>,
    /// The mod's `version` before.
    pub version_before: String,
    /// The mod's `version` after (the old one on failure).
    pub version_after: String,
}

impl ReloadOutcome {
    fn add_note(&mut self, note: String) {
        self.note = Some(match self.note.take() {
            Some(earlier) => format!("{earlier}; {note}"),
            None => note,
        });
    }
}

/// One mod's line in a [`SaveReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModSaveLine {
    /// The mod.
    pub id: String,
    /// Bytes it took in the save; 0 when it was left out.
    pub bytes: u64,
    /// Owned entities the save left out: their `Name` or `Transform` was gone, or they were not
    /// restorable (a non-finite position, an overlong name). Each is warned once.
    #[serde(default)]
    pub skipped_entities: u64,
    /// Why its state was left out (`bytes` is 0), or why `on_save` failed (the state saved as it
    /// stood).
    pub error: Option<ModError>,
}

/// What [`LuaMods::save_state`] wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveReport {
    /// One line per mod, in id order.
    pub mods: Vec<ModSaveLine>,
    /// Entries kept for mods that are not running now.
    pub kept: Vec<String>,
    /// Bytes of the blob.
    pub bytes: u64,
}

/// One mod's line in a [`RestoreReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModRestoreLine {
    /// The mod.
    pub id: String,
    /// What happened to its state; `None` when it failed.
    pub state: Option<StateCarry>,
    /// Why it failed: the mod is disabled and its saved entry is kept.
    pub error: Option<ModError>,
}

/// What [`LuaMods::restore_state`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreReport {
    /// One line per loaded mod, in id order.
    pub mods: Vec<ModRestoreLine>,
    /// Saved entries with no running mod, kept for the next save.
    pub kept: Vec<String>,
    /// Why the blob as a whole could not be read: every mod started fresh.
    pub error: Option<ModError>,
}

/// One loaded mod.
pub struct LuaMod {
    manifest: LuaModManifest,
    files: Arc<ModFiles>,
    limits: LuaLimits,
    lua: Lua,
    budget: Arc<Budget>,
    status: ModStatus,
    counters: ModCounters,
    errors: VecDeque<(String, ModError)>,
    last_reload: Option<ReloadOutcome>,
    /// Files a watched reload refused: not tried again until they change.
    rejected: Option<ModFiles>,
    /// Inside one event delivery pass: every handler call shares one budget and one set of caps.
    delivering: bool,
    /// A hot reload's new version, not yet running: a failure drops it and disables nothing the
    /// game can see, so it is the reload's report and not a warning of its own.
    trial: bool,
    /// The lower entity cap `LuaMods::set_entity_cap` gave this mod; it survives restarts, reloads
    /// and loads (ADR-2418).
    entity_cap: Option<usize>,
}

impl std::fmt::Debug for LuaMod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LuaMod")
            .field("id", &self.manifest.id)
            .field("status", &self.status)
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}

impl LuaMod {
    /// The manifest.
    #[must_use]
    pub fn manifest(&self) -> &LuaModManifest {
        &self.manifest
    }
    /// The mod id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.manifest.id
    }
    /// Whether it runs, or why not.
    #[must_use]
    pub fn status(&self) -> &ModStatus {
        &self.status
    }
    /// Whether it is active.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.status == ModStatus::Active
    }
    /// Its counters.
    #[must_use]
    pub fn counters(&self) -> ModCounters {
        self.counters
    }
    /// Bytes the state holds after a full garbage collection.
    ///
    /// Lua 5.1 collects unreachable values before the read, so temporary vectors from a tick
    /// are not part of this number. A collection that the state refuses still reports the
    /// bytes it holds now.
    #[must_use]
    pub fn memory_after_gc(&self) -> u64 {
        let _ = self.lua.gc_collect();
        self.lua.used_memory() as u64
    }
    /// The limits it runs under (the host's, with its manifest's requests applied).
    #[must_use]
    pub fn limits(&self) -> LuaLimits {
        self.limits
    }
    /// The entity cap it runs under: entities it may own at once (ADR-2418).
    #[must_use]
    pub fn entity_cap(&self) -> usize {
        self.entity_cap.map_or(self.limits.max_entities_per_mod, |cap| {
            cap.min(self.limits.max_entities_per_mod)
        })
    }
    /// The lower cap `LuaMods::set_entity_cap` gave it, if any.
    #[must_use]
    pub fn entity_cap_override(&self) -> Option<usize> {
        self.entity_cap
    }
    /// Spawns its entity cap refused (`lua_entity_cap`).
    #[must_use]
    pub fn entity_cap_refused(&self) -> u64 {
        self.lua
            .app_data_ref::<ModRuntime>()
            .map_or(0, |rt| rt.entity_cap_refused)
    }
    /// While a load or a reload builds this version, `entities` (the previous incarnation's) do not
    /// count against the entity cap; `None` counts everything again.
    fn exclude_from_cap(&self, entities: Option<&[u64]>) {
        if let Some(mut rt) = self.lua.app_data_mut::<ModRuntime>() {
            rt.cap_excluded = entities.map(<[u64]>::to_vec).unwrap_or_default();
            rt.cap_excluded.sort_unstable();
        }
    }
    fn apply_entity_cap(&mut self, cap: Option<usize>) {
        self.entity_cap = cap;
        let effective = self.entity_cap();
        if let Some(mut rt) = self.lua.app_data_mut::<ModRuntime>() {
            rt.entity_cap = effective;
        }
    }
    /// Its recent errors, oldest first, with the call that raised each.
    pub fn errors(&self) -> impl Iterator<Item = (&str, &ModError)> {
        self.errors.iter().map(|(call, e)| (call.as_str(), e))
    }
    /// Its log ring (`print`, `regia.log`), oldest first.
    #[must_use]
    pub fn log(&self) -> Vec<String> {
        self.lua
            .app_data_ref::<ModRuntime>()
            .map(|rt| rt.log.iter().cloned().collect())
            .unwrap_or_default()
    }
    /// Its files.
    #[must_use]
    pub fn files(&self) -> &ModFiles {
        &self.files
    }
    /// The outcome of its last hot reload.
    #[must_use]
    pub fn last_reload(&self) -> Option<&ReloadOutcome> {
        self.last_reload.as_ref()
    }
    /// Event names it subscribed to, sorted.
    #[must_use]
    pub fn subscriptions(&self) -> Vec<String> {
        let Ok(handlers) = self.lua.named_registry_value::<Table>(HANDLERS_KEY) else {
            return Vec::new();
        };
        let mut names: Vec<String> = handlers
            .pairs::<String, Value>()
            .filter_map(Result::ok)
            .map(|(k, _)| k)
            .collect();
        names.sort();
        names
    }

    /// The component declarations this version made with `regia.component`, in declaration order.
    #[must_use]
    pub fn components(&self) -> Vec<ComponentDecl> {
        self.lua
            .app_data_ref::<ModRuntime>()
            .map(|rt| rt.decls.clone())
            .unwrap_or_default()
    }

    /// The side this mod runs on: `regia.command` exists only on [`ScriptAuthority::Host`]
    /// (ADR-8466).
    #[must_use]
    pub fn authority(&self) -> ScriptAuthority {
        self.read_authority(|a| a.authority).unwrap_or_default()
    }

    /// Commands this mod queued with `regia.command` that the game has not drained yet.
    #[must_use]
    pub fn pending_host_commands(&self) -> usize {
        self.read_authority(|a| a.pending.len()).unwrap_or(0)
    }

    /// Commands `regia.command` refused this mod (an unregistered name, a bad or oversized
    /// value, a full queue). A refusal returns `false` and a reason to the script and does not
    /// disable it. Kept across restarts and reloads.
    #[must_use]
    pub fn host_commands_refused(&self) -> u64 {
        self.read_authority(|a| a.refused).unwrap_or(0)
    }

    /// Snapshot tables this mod's state has built: one per published snapshot it read (with
    /// `on_snapshot` or `regia.snapshot()`), none for a snapshot published unchanged.
    #[must_use]
    pub fn snapshot_builds(&self) -> u64 {
        self.read_authority(|a| a.builds).unwrap_or(0)
    }

    fn read_authority<R>(&self, f: impl FnOnce(&ModAuthority) -> R) -> Option<R> {
        self.lua.app_data_ref::<ModRuntime>().map(|rt| f(&rt.authority))
    }

    fn write_authority<R>(&self, f: impl FnOnce(&mut ModAuthority) -> R) -> Option<R> {
        self.lua
            .app_data_mut::<ModRuntime>()
            .map(|mut rt| f(&mut rt.authority))
    }

    /// Takes over the commands `old` queued and nobody drained yet, ahead of this version's own,
    /// and its refusal count: a restart or a reload does not lose a request the game has not seen.
    fn carry_commands(&mut self, old: &LuaMod) {
        let Some((pending, refused)) = old.read_authority(|a| (a.pending.clone(), a.refused)) else {
            return;
        };
        self.write_authority(|a| {
            let own = std::mem::replace(&mut a.pending, pending);
            a.pending.extend(own);
            a.refused += refused;
        });
    }

    /// Takes every waiting command.
    fn take_commands(&mut self) -> VecDeque<HostCommand> {
        self.write_authority(|a| std::mem::take(&mut a.pending))
            .unwrap_or_default()
    }

    /// Drops this version's component declarations so a refused name is not listed or updated.
    fn clear_components(&mut self) {
        if let Some(mut rt) = self.lua.app_data_mut::<ModRuntime>() {
            rt.decls.clear();
        }
        if let Ok(empty) = self.lua.create_table() {
            let _ = self.lua.set_named_registry_value(COMPONENTS_KEY, empty);
        }
    }

    /// Runs the `on_update(entity, dt)` hook of every declared component that has one, once per
    /// entity carrying the component, in declaration order. All of a mod's passes in one update
    /// are one call as far as the budgets go, like an event delivery: they share one instruction
    /// budget, one watchdog and the emit and spawn caps, so a mod cannot multiply its budget by
    /// declaring 64 components (ADR-5870). A pass with no entities makes no call and charges
    /// nothing (spec 84 § 14, ADR-5816); a pass that has entities charges its world scan to that
    /// shared budget. A failing hook disables the mod like any call.
    fn update_components(&mut self, world: &mut dyn ModWorld, dt: f64) {
        let names: Vec<String> = self
            .lua
            .app_data_ref::<ModRuntime>()
            .map(|rt| {
                rt.decls
                    .iter()
                    .filter(|decl| decl.has_update)
                    .map(|decl| decl.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        if names.is_empty() {
            return;
        }
        let registry = match self.lua.named_registry_value::<Table>(COMPONENTS_KEY) {
            Ok(registry) => registry,
            Err(_) => return,
        };
        self.begin_budget();
        self.delivering = true;
        for name in names {
            if !self.is_active() {
                break;
            }
            let Ok(table) = registry.raw_get::<Table>(name.as_str()) else {
                continue;
            };
            let Ok(Value::Function(handler)) = table.raw_get("on_update") else {
                continue;
            };
            let entities = world.entities_with(&name);
            if entities.is_empty() {
                // Spec 84 § 14 (ADR-5816): an empty pass makes no call and does not touch the
                // budget, so a mod whose budget is under the base charge keeps running.
                continue;
            }
            // The world scan is charged inside this call's fresh budget, not against the
            // previous call's counter.
            let scan = COMPONENT_BASE_COST
                .saturating_add(world.component_cost() / COMPONENT_ENTITIES_PER_INSTRUCTION)
                .saturating_add(entities.len() as u64);
            let _ = self.call_charged(world, "on_update", scan, move |lua| {
                for bits in entities {
                    let handle = crate::value::entity_value(lua, bits)?;
                    handler.call::<()>((handle, dt))?;
                }
                Ok(())
            });
        }
        self.delivering = false;
    }

    /// Builds the state and installs the host API; nothing of the mod runs yet.
    fn build(
        manifest: LuaModManifest,
        files: Arc<ModFiles>,
        host: &LuaLimits,
        seed: u64,
        side: &HostSide,
    ) -> Result<Self, ModError> {
        let limits = host.for_mod(manifest.instruction_budget, manifest.memory_limit_bytes);
        let sandbox = sandbox::new_state(&limits)?;
        install_api(
            &sandbox.lua,
            &manifest.id,
            &files,
            &limits,
            seed,
            &sandbox.budget,
            side,
        )
        .map_err(|e| ModError::new(LuaErrorCode::Runtime, format!("installing the host API: {e}")))?;
        sandbox::seal(&sandbox.lua, &limits)?;
        Ok(Self {
            manifest,
            files,
            limits,
            lua: sandbox.lua,
            budget: sandbox.budget,
            status: ModStatus::Active,
            counters: ModCounters::default(),
            errors: VecDeque::new(),
            last_reload: None,
            rejected: None,
            delivering: false,
            trial: false,
            entity_cap: None,
        })
    }

    /// Takes over what must survive a rebuild from the version it replaces.
    fn inherit(&mut self, old: &LuaMod) {
        self.apply_entity_cap(old.entity_cap);
        if let (Some(mut rt), Some(before)) = (
            self.lua.app_data_mut::<ModRuntime>(),
            old.lua.app_data_ref::<ModRuntime>(),
        ) {
            rt.entity_cap_refused = before.entity_cap_refused;
        }
        self.errors = old.errors.clone();
        self.counters.calls = old.counters.calls;
        self.counters.reloads = old.counters.reloads;
        self.counters.failed_reloads = old.counters.failed_reloads;
        self.last_reload = old.last_reload.clone();
    }

    /// Runs the entry chunk, then `on_start` when the mod defines it.
    fn start(&mut self, world: &mut dyn ModWorld) -> Result<(), ModError> {
        self.run_entry(world)?;
        self.call_global(world, "on_start", ())
    }

    /// Runs the entry chunk.
    fn run_entry(&mut self, world: &mut dyn ModWorld) -> Result<(), ModError> {
        let entry = self.manifest.entry.clone();
        let Some(source) = self.files.get(&entry).map(str::to_owned) else {
            let error = ModError::new(
                LuaErrorCode::Manifest,
                format!("entry `{entry}` is not in the mod"),
            );
            self.fail("load", error.clone());
            return Err(error);
        };
        let chunk_name = format!("{}/{entry}", self.manifest.id);
        self.call(world, "load", |lua| {
            sandbox::load_text(lua, &source, &chunk_name)?.call::<()>(())
        })
    }

    /// Calls global function `name` when the mod defines one.
    fn call_global(
        &mut self,
        world: &mut dyn ModWorld,
        name: &'static str,
        args: impl mlua::IntoLuaMulti + Clone,
    ) -> Result<(), ModError> {
        let Some(f) = self.global_fn(name) else {
            return Ok(());
        };
        self.call(world, name, move |_| f.call::<()>(args))
    }

    /// Global function `name`, when the mod defines one.
    fn global_fn(&self, name: &str) -> Option<Function> {
        match self.lua.globals().raw_get::<Value>(name) {
            Ok(Value::Function(f)) => Some(f),
            _ => None,
        }
    }

    /// One call into the mod under its budget, with `world` reachable from the `regia` table for
    /// the call's duration. Any error disables the mod.
    fn call<R>(
        &mut self,
        world: &mut dyn ModWorld,
        label: &str,
        f: impl FnOnce(&Lua) -> mlua::Result<R>,
    ) -> Result<R, ModError> {
        self.call_charged(world, label, 0, f)
    }

    /// [`call`](Self::call) that bills `charge` instructions on the fresh per-call budget,
    /// before the closure runs. A charge the budget cannot hold disables the mod and does not
    /// run the closure.
    fn call_charged<R>(
        &mut self,
        world: &mut dyn ModWorld,
        label: &str,
        charge: u64,
        f: impl FnOnce(&Lua) -> mlua::Result<R>,
    ) -> Result<R, ModError> {
        if let ModStatus::Disabled { code, message } = &self.status {
            return Err(ModError::new(
                LuaErrorCode::Disabled,
                format!("the mod is disabled ({code}: {message})"),
            ));
        }
        self.run_call(world, label, charge, f).inspect_err(|error| {
            self.fail(label, error.clone());
        })
    }

    /// [`call`](Self::call) without the disabling: the caller decides what an error means.
    fn run_call<R>(
        &mut self,
        world: &mut dyn ModWorld,
        label: &str,
        charge: u64,
        f: impl FnOnce(&Lua) -> mlua::Result<R>,
    ) -> Result<R, ModError> {
        let _span = tracing::debug_span!("lua_mod.call", id = %self.manifest.id, call = label).entered();
        if self.delivering {
            self.budget
                .charge(HANDLER_CALL_COST.saturating_add(charge))
                .map_err(|e| ModError::new(LuaErrorCode::InstructionBudget, e.to_string()))?;
            self.budget.start_clock();
        } else {
            self.begin_budget();
            if charge > 0 {
                self.budget
                    .charge(charge)
                    .map_err(|e| ModError::new(LuaErrorCode::InstructionBudget, e.to_string()))?;
            }
        }
        // A call that fails takes the commands it queued with it (ADR-8466): the game never sees
        // half of what a failed hook meant to ask for.
        let pending_before = self.pending_host_commands();
        let lua = &self.lua;
        let owner = self.manifest.id.clone();
        let result = lua.scope(|scope| {
            let dispatcher =
                scope.create_function_mut(move |lua, (op, lo, hi, a, b): (u8, u32, u32, Value, Value)| {
                    let entity = u64::from(lo) | (u64::from(hi) << 32);
                    dispatch(lua, world, &owner, op, entity, &a, &b)
                })?;
            lua.set_named_registry_value(HOST_KEY, dispatcher)?;
            let out = f(lua);
            lua.set_named_registry_value(HOST_KEY, Value::Nil)?;
            out
        });
        if self.delivering {
            self.budget.stop_clock();
        }
        let used = self.budget.used();
        self.counters.calls += 1;
        self.counters.last_instructions = used;
        self.counters.peak_instructions = self.counters.peak_instructions.max(used);
        let memory = self.lua.used_memory() as u64;
        self.counters.memory_bytes = memory;
        self.counters.peak_memory_bytes = self.counters.peak_memory_bytes.max(memory);
        match result {
            Ok(r) => Ok(r),
            Err(e) => {
                self.write_authority(|a| a.pending.truncate(pending_before));
                let code = if self.budget.tripped() {
                    LuaErrorCode::InstructionBudget
                } else if self.budget.memory_tripped() {
                    LuaErrorCode::Memory
                } else {
                    classify(&e)
                };
                Err(ModError::new(code, e.to_string()))
            }
        }
    }

    /// Fresh per-call caps and instruction budget.
    fn begin_budget(&self) {
        if let Some(mut rt) = self.lua.app_data_mut::<ModRuntime>() {
            rt.emits_this_call = 0;
            rt.emit_bytes_this_call = 0;
            rt.spawns_this_call = 0;
            rt.call_log.clear();
        }
        self.budget.begin(self.limits.instruction_budget);
    }

    /// Records `error` and disables the mod.
    fn fail(&mut self, label: &str, error: ModError) {
        if !self.trial {
            tracing::warn!(target: "regia_script_lua", id = %self.manifest.id, call = label, code = error.code.code(), "mod disabled: {}", error.message);
        }
        self.note_error(label, error.clone());
        self.status = ModStatus::Disabled {
            code: error.code,
            message: error.message,
        };
    }

    /// Records `error` in the ring; the mod keeps its status.
    fn note_error(&mut self, label: &str, error: ModError) {
        if self.errors.len() >= MAX_ERRORS_KEPT {
            self.errors.pop_front();
        }
        self.errors.push_back((label.to_owned(), error));
    }

    /// The `regia` table.
    fn regia(&self) -> Result<Table, ModError> {
        self.lua
            .named_registry_value::<Table>(REGIA_KEY)
            .map_err(|e| ModError::new(LuaErrorCode::Runtime, e.to_string()))
    }

    /// `regia.state_version` (1 when the mod sets none).
    fn state_version(&self) -> Result<u32, ModError> {
        let value = self
            .regia()?
            .raw_get::<Value>("state_version")
            .map_err(|e| ModError::new(LuaErrorCode::Runtime, e.to_string()))?;
        state::state_version_of(&value)
    }

    /// Reads `regia.state`, the mod's random number generator and (with `world`) its entities,
    /// without running any Lua, plus how many owned entities the save left out (their `Name` or
    /// `Transform` was gone, or they were not restorable). The memory cap is lifted for the
    /// read: a state at its cap must still be able to save, and the read keeps nothing in the
    /// state.
    fn capture(&self, world: Option<&mut dyn ModWorld>) -> Result<(ModState, u64), ModError> {
        let internal = |e: mlua::Error| ModError::new(LuaErrorCode::Runtime, e.to_string());
        let regia = self.regia()?;
        let cap = self.lua.set_memory_limit(usize::MAX).map_err(internal)?;
        let tree = match regia.raw_get::<Value>("state") {
            Ok(Value::Table(root)) => StateTree::capture(&root, &self.limits),
            Ok(Value::Nil) => Ok(StateTree::default()),
            Ok(other) => Err(ModError::new(
                LuaErrorCode::BadValue,
                format!("regia.state must be a table, not a {}", lua_type(&other)),
            )),
            Err(e) => Err(internal(e)),
        };
        self.lua.set_memory_limit(cap).map_err(internal)?;
        let tree = tree?;
        let mut entities = Vec::new();
        let mut skipped = 0u64;
        if let Some(world) = world {
            for entity in world.owned(&self.manifest.id) {
                if let (Some(name), Some(position)) = (world.name(entity), world.position(entity)) {
                    let saved = SavedModEntity {
                        name,
                        position: position.to_array(),
                    };
                    if saved.restorable() {
                        entities.push(saved);
                        continue;
                    }
                    tracing::warn!(
                        target: "regia_script_lua",
                        id = %self.manifest.id,
                        name = %saved.name,
                        "an owned entity with a non-finite position or an overlong name is left out of the save"
                    );
                } else {
                    // A pooling system that strips the Name or the Transform must not make the
                    // entity vanish silently: the save report counts it (and warns).
                    tracing::warn!(
                        target: "regia_script_lua",
                        id = %self.manifest.id,
                        entity,
                        "an owned entity without a Name or a Transform is left out of the save"
                    );
                }
                skipped += 1;
            }
        }
        if entities.len() > self.limits.max_state_entities {
            return Err(ModError::new(
                LuaErrorCode::TooLarge,
                format!(
                    "the mod owns {} entities, more than the {} a save keeps",
                    entities.len(),
                    self.limits.max_state_entities
                ),
            ));
        }
        let saved = ModState {
            id: self.manifest.id.clone(),
            mod_version: self.manifest.version.clone(),
            state_version: self.state_version()?,
            rng: self.rng(),
            tree,
            entities,
        };
        let bytes = state::encoded_size(&saved)?;
        if bytes > self.limits.max_state_bytes {
            return Err(ModError::new(
                LuaErrorCode::TooLarge,
                format!(
                    "the mod's state is {bytes} bytes, more than the {} allowed",
                    self.limits.max_state_bytes
                ),
            ));
        }
        Ok((saved, skipped))
    }

    /// Fills `regia.state` from `tree`. The table stays the one the entry chunk saw, emptied and
    /// refilled, because a chunk may have kept it in a local: replacing it would leave that local
    /// on a table no save reads. A table with a metatable cannot be saved, so that one is replaced.
    fn set_state(&self, tree: &StateTree) -> mlua::Result<()> {
        let regia = self.lua.named_registry_value::<Table>(REGIA_KEY)?;
        let root = match regia.raw_get::<Value>("state")? {
            Value::Table(table) if table.metatable().is_none() => table,
            _ => self.lua.create_table()?,
        };
        regia.raw_set("state", tree.build_into(&self.lua, root)?)
    }

    /// Hands `saved` to the freshly run new version: as it is when the versions match, through
    /// `on_migrate` when they differ. Without an `on_migrate` the new version keeps what its entry
    /// chunk set up.
    fn adopt(&mut self, world: &mut dyn ModWorld, saved: &ModState) -> Result<StateCarry, ModError> {
        let from = saved.state_version;
        let to = self.state_version()?;
        if from == to {
            self.set_state(&saved.tree)
                .map_err(|e| ModError::new(classify(&e), e.to_string()))?;
            return Ok(StateCarry::Carried);
        }
        let Some(migrate) = self.global_fn("on_migrate") else {
            return Ok(StateCarry::Reset { from, to });
        };
        let internal = |e: mlua::Error| ModError::new(classify(&e), e.to_string());
        let regia = self.regia()?;
        let chunk_state = regia.raw_get::<Value>("state").map_err(internal)?;
        let tree = &saved.tree;
        self.call(world, "on_migrate", move |lua| {
            let old = tree.build(lua)?;
            let migrated = match migrate.call::<Value>((old.clone(), from, to))? {
                Value::Nil => old,
                Value::Table(new) => new,
                other => {
                    return Err(HostError::lua(
                        LuaErrorCode::BadValue,
                        format!(
                            "on_migrate must return a table or nothing, not a {}",
                            lua_type(&other)
                        ),
                    ));
                }
            };
            lua.named_registry_value::<Table>(REGIA_KEY)?
                .raw_set("state", migrated)
        })?;
        // A migrator's result has to be state a save can hold. It goes through the plain tree into
        // the chunk's own table, like a state that needed no migrator.
        let (migrated, _) = self.capture(None)?;
        regia.raw_set("state", chunk_state).map_err(internal)?;
        self.lua.gc_collect().map_err(internal)?;
        self.set_state(&migrated.tree).map_err(internal)?;
        Ok(StateCarry::Migrated { from, to })
    }

    /// Calls `on_load(info)`.
    fn call_on_load(
        &mut self,
        world: &mut dyn ModWorld,
        on_load: Function,
        reason: &'static str,
        carry: StateCarry,
    ) -> Result<(), ModError> {
        let (from, to) = match carry {
            StateCarry::Migrated { from, to } | StateCarry::Reset { from, to } => (Some(from), Some(to)),
            StateCarry::Fresh | StateCarry::Carried => (None, None),
        };
        self.call(world, "on_load", move |lua| {
            let info = lua.create_table()?;
            info.raw_set("reason", reason)?;
            info.raw_set("state", carry.name())?;
            info.raw_set("from_version", from)?;
            info.raw_set("to_version", to)?;
            on_load.call::<()>(info)
        })
    }

    /// Restores this freshly built mod from a save entry (`None`: it was not in the save), after
    /// `owned` (its entities from before the load) are gone.
    fn revive(
        &mut self,
        world: &mut dyn ModWorld,
        saved: Option<&ModState>,
        owned: &[u64],
    ) -> Result<StateCarry, ModError> {
        let id = self.manifest.id.clone();
        // The entities from before the load go below: until then they do not count against the
        // entity cap, or a mod at its cap could never load (ADR-2418).
        self.exclude_from_cap(Some(owned));
        let built = self.run_entry(world).and_then(|()| match saved {
            Some(saved) => {
                let carry = self.adopt(world, saved)?;
                self.set_rng(&saved.rng);
                Ok(carry)
            }
            None => Ok(StateCarry::Fresh),
        });
        self.exclude_from_cap(None);
        let carry = built?;
        for entity in owned {
            world.despawn(&id, *entity);
        }
        match (saved, self.global_fn("on_load")) {
            (Some(saved), Some(on_load)) => {
                // The saved list is the mod's entities: what the entry chunk and `on_migrate`
                // spawned during this load goes too, or every load adds a copy (audit wave 6,
                // luasave M2).
                for entity in world.owned(&id) {
                    world.despawn(&id, entity);
                }
                for entity in &saved.entities {
                    world.spawn(&id, &entity.name, Vec3::from_array(entity.position));
                }
                self.call_on_load(world, on_load, "save", carry)?;
            }
            _ => self.call_global(world, "on_start", ())?,
        }
        Ok(carry)
    }

    fn rng(&self) -> Rng {
        self.lua
            .app_data_ref::<ModRuntime>()
            .map(|rt| rt.rng.clone())
            .unwrap_or_default()
    }

    fn set_rng(&self, rng: &Rng) {
        if let Some(mut rt) = self.lua.app_data_mut::<ModRuntime>() {
            rt.rng = rng.clone();
        }
    }

    /// Compiles every `.lua` file, so a syntax error in a module the entry chunk has not required
    /// yet stops a reload before anything runs.
    fn compile_all(&self) -> Result<(), ModError> {
        for path in self.files.paths().filter(|path| path.ends_with(".lua")) {
            let source = self.files.get(path).unwrap_or_default();
            sandbox::load_text(&self.lua, source, &format!("{}/{path}", self.manifest.id))
                .map_err(|e| ModError::new(classify(&e), e.to_string()))?;
        }
        Ok(())
    }

    /// The new version's side of a hot reload once its entry chunk has run: the state, the random
    /// numbers, then `on_load` or a restart that takes `owned`, the entities from before it.
    fn take_over(
        &mut self,
        journal: &mut JournalWorld<'_>,
        saved: Option<&ModState>,
        rng: &Rng,
        owned: Vec<u64>,
        from_version: Option<u32>,
        outcome: &mut ReloadOutcome,
    ) -> Result<(), ModError> {
        let id = self.manifest.id.clone();
        outcome.stage = ReloadStage::State;
        let to = self.state_version()?;
        let carry = match saved {
            Some(saved) => self.adopt(journal, saved)?,
            // The old state was not plain data: `outcome.note` says why.
            None => StateCarry::Reset {
                from: from_version.unwrap_or(to),
                to,
            },
        };
        self.set_rng(saved.map_or(rng, |saved| &saved.rng));
        outcome.state = Some(carry);
        outcome.stage = ReloadStage::Start;
        // From here the entities the mod keeps are its own again, and count.
        self.exclude_from_cap(None);
        match self.global_fn("on_load") {
            Some(on_load) => {
                // The previous version's entities stay; what this version's entry chunk and
                // `on_migrate` just spawned goes, or each reload adds a copy (audit wave 6,
                // luasave M2).
                let before: std::collections::HashSet<u64> = owned.into_iter().collect();
                for entity in journal.owned(&id) {
                    if !before.contains(&entity) {
                        journal.despawn(&id, entity);
                    }
                }
                self.call_on_load(journal, on_load, "reload", carry)
            }
            None => {
                for entity in owned {
                    journal.despawn(&id, entity);
                }
                self.call_global(journal, "on_start", ())
            }
        }
    }

    /// The events the mod emitted since the last take.
    fn take_outbox(&self) -> Vec<EmittedEvent> {
        self.lua
            .app_data_mut::<ModRuntime>()
            .map(|mut rt| std::mem::take(&mut rt.outbox))
            .unwrap_or_default()
    }

    /// The handlers subscribed to `name`, in subscription order.
    fn handlers(&self, name: &str) -> Vec<Function> {
        let Ok(handlers) = self.lua.named_registry_value::<Table>(HANDLERS_KEY) else {
            return Vec::new();
        };
        let Ok(list) = handlers.raw_get::<Table>(name) else {
            return Vec::new();
        };
        list.sequence_values::<Function>()
            .filter_map(Result::ok)
            .collect()
    }
}

/// Events waiting for [`LuaMods::drain_emitted`], bounded because nothing guarantees a reader.
#[derive(Debug, Default)]
struct EmittedQueue {
    events: VecDeque<(EmittedEvent, u64)>,
    bytes: u64,
    dropped: u64,
}

impl EmittedQueue {
    fn hold(&mut self, events: Vec<EmittedEvent>) {
        for event in events {
            let weight = event.value.weight();
            self.bytes += weight;
            self.events.push_back((event, weight));
        }
        let first = self.dropped == 0;
        while self.events.len() > MAX_EMITTED_HELD || self.bytes > MAX_EMITTED_HELD_BYTES {
            let Some((_, weight)) = self.events.pop_front() else {
                break;
            };
            self.bytes -= weight;
            self.dropped += 1;
        }
        if first && self.dropped > 0 {
            tracing::warn!(target: "regia_script_lua", "mod events are not being drained: the oldest are dropped");
        }
    }

    fn take(&mut self) -> Vec<EmittedEvent> {
        self.bytes = 0;
        std::mem::take(&mut self.events)
            .into_iter()
            .map(|(event, _)| event)
            .collect()
    }

    fn clear(&mut self) {
        self.events.clear();
        self.bytes = 0;
    }
}

/// Every loaded mod, in id order, plus the event queues. Insert with [`crate::LuaModsPlugin`] or
/// by hand; calls take a [`ModWorld`].
#[derive(Resource, Debug)]
pub struct LuaMods {
    seed: u64,
    limits: LuaLimits,
    mods: Vec<LuaMod>,
    queued: Vec<(String, ScriptValue)>,
    emitted: EmittedQueue,
    /// Saved entries with no running mod, by id; written back by the next save.
    kept: BTreeMap<String, ModState>,
    last_save: Option<SaveReport>,
    last_restore: Option<RestoreReport>,
    /// Frames between two looks at the mods' directories; 0 is off.
    watch_every: u32,
    watch_left: u32,
    /// Manifest ids a second mount claimed this session (ADR-7881, `host/mount.rs`).
    contested: std::collections::BTreeSet<String>,
    /// Saved entries withheld from Workshop or contested claims, by manifest id. Every
    /// successful save writes them back unchanged (ADR-7951, narrowed by ADR-7992).
    held: BTreeMap<String, ModState>,
    /// Restored entry ids that contain `:`. They are not v1 manifest ids, so they are
    /// quarantined and reported once instead of failing later saves (ADR-7992).
    colon_skipped: Vec<String>,
    /// The host or presentation side, the command rules and the latest snapshot (ADR-8466).
    side: HostSide,
}

impl LuaMods {
    /// An empty host. `seed` seeds every mod's random numbers (with the mod id as the stream).
    ///
    /// ```
    /// use regia_script_lua::{LuaLimits, LuaMods};
    ///
    /// // The seed makes `math.random` repeatable; a mod may ask for smaller budgets, never larger.
    /// let mods = LuaMods::new(7, LuaLimits::default());
    /// assert_eq!(mods.seed(), 7);
    /// assert_eq!(mods.limits(), LuaLimits::default());
    /// assert_eq!(mods.iter().count(), 0);
    /// ```
    #[must_use]
    pub fn new(seed: u64, limits: LuaLimits) -> Self {
        Self {
            seed,
            limits,
            mods: Vec::new(),
            queued: Vec::new(),
            emitted: EmittedQueue::default(),
            kept: BTreeMap::new(),
            last_save: None,
            last_restore: None,
            watch_every: 0,
            watch_left: 0,
            contested: std::collections::BTreeSet::new(),
            held: BTreeMap::new(),
            colon_skipped: Vec::new(),
            side: HostSide::default(),
        }
    }

    /// The host seed.
    #[must_use]
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// The host limits.
    #[must_use]
    pub fn limits(&self) -> LuaLimits {
        self.limits
    }

    /// Gives mod `id` its own entity cap, at most the host's `LuaLimits::max_entities_per_mod`
    /// (`None` goes back to the host's). It holds across restarts, reloads and loads. Entities the
    /// mod already owns stay; only later spawns are refused (`lua_entity_cap`). Returns the cap the
    /// mod now runs under.
    ///
    /// # Errors
    /// `lua_unknown_mod`.
    pub fn set_entity_cap(&mut self, id: &str, cap: Option<usize>) -> Result<usize, ModError> {
        let index = self.index(id).ok_or_else(|| unknown(id))?;
        let module = &mut self.mods[index];
        module.apply_entity_cap(cap);
        Ok(module.entity_cap())
    }

    /// Loads a mod: checks its manifest and size, builds its state, runs its entry chunk and
    /// `on_start`. Returns its id. A manifest or size error loads nothing; an error in the
    /// entry chunk or `on_start` keeps the mod, disabled, and returns that error. A component
    /// name another loaded mod already declared does the same (`lua_bad_value`): the declaration
    /// is not registered, and a fresh load rolls the entry chunk's world writes back.
    ///
    /// ```
    /// use regia_script_lua::{LuaLimits, LuaMods, ModFiles, ModStatus, NoWorld};
    ///
    /// let manifest = r#"(format_version: 1, id: "demo", name: "Demo", version: "1.0.0", entry: "main.lua", api_version: 1)"#;
    /// let mut mods = LuaMods::new(1, LuaLimits::default());
    /// let files = ModFiles::from_map([("luamod.ron", manifest), ("main.lua", "function on_tick(dt) error('boom') end")]);
    /// assert_eq!(mods.load(files, &mut NoWorld).unwrap(), "demo");
    /// assert_eq!(mods.get("demo").unwrap().status(), &ModStatus::Active);
    ///
    /// // An error in a callback disables that mod only; the host keeps running.
    /// mods.call_tick(&mut NoWorld, 0.016);
    /// assert!(matches!(mods.get("demo").unwrap().status(), ModStatus::Disabled { .. }));
    /// ```
    pub fn load(&mut self, files: ModFiles, world: &mut dyn ModWorld) -> Result<String, ModError> {
        let manifest = check_files(&files, &self.limits)?;
        let id = manifest.id.clone();
        // The id is a claim: another mount holding it refuses this one (ADR-7881).
        self.claim_mount(&id, &files)?;
        let slot = match self.mods.binary_search_by(|m| m.id().cmp(&id)) {
            Ok(_) => {
                return Err(ModError::new(
                    LuaErrorCode::Manifest,
                    format!("a mod with id `{id}` is already loaded"),
                ));
            }
            Err(slot) => slot,
        };
        let mut module = LuaMod::build(manifest, Arc::new(files), &self.limits, self.seed, &self.side)?;
        // A save loaded before this mod was: its entry waits in `kept`. It goes back there
        // unless the load fully succeeds (a failed revive or a refused declaration re-keeps it).
        let mut saved_entry = self.kept.remove(&id);
        let started = match saved_entry.as_ref() {
            Some(saved) => {
                let owned = world.owned(&id);
                let revived = saved
                    .check(&self.limits)
                    .and_then(|()| module.revive(world, Some(saved), &owned));
                if let Err(error) = &revived {
                    if module.is_active() {
                        module.fail("restore", error.clone());
                    }
                    // What the failed revive spawned goes, as in `restore_state`.
                    for entity in world.owned(&id) {
                        world.despawn(&id, entity);
                    }
                }
                revived.map(|_| ())
            }
            None => {
                let mut journal = JournalWorld::new(world, &id);
                let started = module.start(&mut journal);
                if started.is_ok()
                    && let Some(error) = component_name_clash(&self.mods, &module, None)
                {
                    journal.rollback();
                    module.fail("load", error.clone());
                    module.clear_components();
                    self.emitted.hold(module.take_outbox());
                    self.mods.insert(slot, module);
                    return Err(error);
                }
                // Spawns and component writes already reached the world. A despawn of an entity
                // this mod owned before the entry chunk is held until commit, which is what a
                // direct call used to do immediately. A failed start keeps what its chunk wrote,
                // so it keeps its despawns too (dropping the journal lost them, as in `restart`).
                journal.commit();
                started.map(|_| ())
            }
        };
        if started.is_err()
            && let Some(saved) = saved_entry.take()
        {
            // A failed revive keeps its entry for the next save, as `restore_state` does.
            self.kept.insert(id.clone(), saved);
        }
        if started.is_ok() {
            // Component names are global. A fresh load already returned above. A restore can
            // still re-declare another mod's component: drop the declaration and stay disabled,
            // and leave the world and the save as they were — what the revive spawned goes (the
            // failed revive above rolls the same way) and the entry it adopted waits in `kept`
            // again, so all three load paths behave alike.
            if let Some(error) = component_name_clash(&self.mods, &module, None) {
                module.fail("load", error.clone());
                module.clear_components();
                for entity in world.owned(&id) {
                    world.despawn(&id, entity);
                }
                if let Some(saved) = saved_entry.take() {
                    self.kept.insert(id.clone(), saved);
                }
                self.emitted.hold(module.take_outbox());
                self.mods.insert(slot, module);
                return Err(error);
            }
            reconcile_components(&module, world);
        }
        self.emitted.hold(module.take_outbox());
        self.mods.insert(slot, module);
        started.map(|()| id)
    }

    /// Rebuilds mod `id` from its files (a fresh state, errors cleared from its status but kept
    /// in its error ring) and starts it again.
    pub fn restart(&mut self, id: &str, world: &mut dyn ModWorld) -> Result<(), ModError> {
        let Some(index) = self.index(id) else {
            return Err(unknown(id));
        };
        let (manifest, files) = {
            let old = &self.mods[index];
            (old.manifest.clone(), Arc::clone(&old.files))
        };
        let mut fresh = LuaMod::build(manifest, files, &self.limits, self.seed, &self.side)?;
        {
            let old = &self.mods[index];
            fresh.inherit(old);
            fresh.carry_commands(old);
        }
        let started = {
            let mut journal = JournalWorld::new(world, id);
            // The entities the old run spawned go before `on_start` spawns its own again. The
            // journal holds the despawns, so a rollback below keeps them.
            for entity in journal.owned(id) {
                journal.despawn(id, entity);
            }
            let started = fresh.start(&mut journal);
            if started.is_ok()
                && let Some(error) = component_name_clash(&self.mods, &fresh, Some(id))
            {
                journal.rollback();
                fresh.fail("restart", error.clone());
                fresh.clear_components();
                self.emitted.hold(fresh.take_outbox());
                self.mods[index] = fresh;
                return Err(error);
            }
            // The old run's entities go whether or not `on_start` succeeds, like a failed load
            // keeps what its entry chunk wrote. Dropping the journal on an error kept the old
            // entities next to the failed run's own spawns.
            journal.commit();
            started
        };
        if started.is_ok() {
            reconcile_components(&fresh, world);
        }
        self.emitted.hold(fresh.take_outbox());
        self.mods[index] = fresh;
        started
    }

    /// Hot reload (`docs/specs/84-lua-mods.md` § 12): replaces mod `id`'s code while it runs.
    /// `files` are the new files; `None` reads the directory the mod was loaded from.
    ///
    /// The new version is built beside the old one and `regia.state` crosses over, through
    /// `on_migrate` when `regia.state_version` changed. Any failure, in any stage, drops the new
    /// version, undoes its world writes and leaves the old one running as it was: the outcome
    /// names the stage and the error. A mod that defines `on_load` keeps its entities; one that
    /// does not restarts (its entities go and `on_start` runs), with its state carried.
    ///
    /// # Errors
    /// `lua_unknown_mod` only: everything else is in the outcome.
    pub fn reload(
        &mut self,
        id: &str,
        files: Option<ModFiles>,
        world: &mut dyn ModWorld,
    ) -> Result<ReloadOutcome, ModError> {
        let index = self.index(id).ok_or_else(|| unknown(id))?;
        let files = self.keep_mount(index, files);
        let _span = tracing::info_span!("lua_mods.reload", id).entered();
        let limits = self.limits;
        let seed = self.seed;
        let side = self.side.clone();
        let reserved = declared_names(&self.mods, Some(id));
        let old = &mut self.mods[index];
        let version_before = old.manifest.version.clone();
        let mut outcome = ReloadOutcome {
            id: id.to_owned(),
            ok: false,
            stage: ReloadStage::Files,
            state: None,
            note: None,
            error: None,
            version_after: version_before.clone(),
            version_before,
        };
        let replaced = replace_mod(
            old,
            files,
            self.kept.get(id),
            &reserved,
            &ReloadSeed {
                limits: &limits,
                seed,
                side: &side,
            },
            world,
            &mut outcome,
        );
        match replaced {
            Ok(mut fresh) => {
                self.kept.remove(id);
                outcome.ok = true;
                outcome.stage = ReloadStage::Done;
                outcome.version_after = fresh.manifest.version.clone();
                fresh.inherit(old);
                fresh.carry_commands(old);
                fresh.counters.reloads += 1;
                fresh.last_reload = Some(outcome.clone());
                self.emitted.hold(old.take_outbox());
                self.emitted.hold(fresh.take_outbox());
                self.mods[index] = fresh;
                tracing::info!(target: "regia_script_lua", id, state = ?outcome.state, "mod reloaded");
            }
            Err(error) => {
                tracing::warn!(target: "regia_script_lua", id, stage = outcome.stage.name(), code = error.code.code(), "reload failed, the old version keeps running: {}", error.message);
                outcome.state = None;
                outcome.error = Some(error.clone());
                // What the old version's `on_save` sent belongs to the reload that did not happen.
                old.take_outbox();
                old.note_error("reload", error);
                old.counters.failed_reloads += 1;
                old.last_reload = Some(outcome.clone());
            }
        }
        Ok(outcome)
    }

    /// How many frames pass between two looks at the mods' directories; 0 is off.
    #[must_use]
    pub fn watch_every(&self) -> u32 {
        self.watch_every
    }

    /// Reloads mods read from a directory when their files change, looking every `frames` frames
    /// (0 turns it off). Off by default. Meant for development: it reads the files on the game
    /// thread.
    pub fn set_watch(&mut self, frames: u32) {
        self.watch_every = frames;
        self.watch_left = frames;
    }

    /// Counts one frame down and looks at the directories when the watch interval is up.
    pub fn tick_watch(&mut self, world: &mut dyn ModWorld) -> Vec<ReloadOutcome> {
        if self.watch_every == 0 {
            return Vec::new();
        }
        self.watch_left = self.watch_left.saturating_sub(1);
        if self.watch_left > 0 {
            return Vec::new();
        }
        self.watch_left = self.watch_every;
        self.poll_files(world)
    }

    /// Reads every directory-backed mod's files once and reloads those that changed. Files a
    /// reload refused are not tried again until they change, so a broken save reports once.
    pub fn poll_files(&mut self, world: &mut dyn ModWorld) -> Vec<ReloadOutcome> {
        let _span = tracing::info_span!("lua_mods.watch").entered();
        let max_bytes = self.limits.max_mod_bytes;
        let mut outcomes = Vec::new();
        for index in 0..self.mods.len() {
            let Some(dir) = self.mods[index]
                .files
                .source_dir()
                .map(std::path::Path::to_path_buf)
            else {
                continue;
            };
            // Size and mtime first. A match, including a rejected save that has not changed,
            // reads no file bodies. A peek that fails falls through to the full read.
            // The two stamps are separate borrows, so they are checked one at a time.
            let current_matches = self.mods[index].files.stamp_matches_dir(&dir).unwrap_or(false);
            let rejected_matches = self.mods[index]
                .rejected
                .as_ref()
                .is_some_and(|rejected| rejected.stamp_matches_dir(&dir).unwrap_or(false));
            if current_matches || rejected_matches {
                continue;
            }
            let files = match ModFiles::from_dir(&dir, max_bytes) {
                Ok(files) => files,
                Err(error) => {
                    // Mid-save, or a deleted directory: the running version stays.
                    tracing::debug!(target: "regia_script_lua", id = self.mods[index].id(), "reading the mod's files: {error}");
                    continue;
                }
            };
            // Equality ignores the stamp. A save that changed no text (a touch, an editor saving
            // an unchanged buffer) keeps the text and takes the new stamp, so the next poll peeks
            // again. Keeping the old stamp reread every body on every poll from then on.
            if files == *self.mods[index].files {
                self.mods[index].files = Arc::new(files);
                continue;
            }
            if self.mods[index].rejected.as_ref() == Some(&files) {
                self.mods[index].rejected = Some(files);
                continue;
            }
            let id = self.mods[index].id().to_owned();
            if let Ok(outcome) = self.reload(&id, Some(files.clone()), world) {
                if !outcome.ok {
                    self.mods[index].rejected = Some(files);
                }
                outcomes.push(outcome);
            }
        }
        outcomes
    }

    /// The whole save failed. Empty bytes, and `error` on every line the caller can see.
    fn refuse_save(
        &mut self,
        mut report: SaveReport,
        file: &LuaStateFile,
        error: ModError,
    ) -> (Vec<u8>, SaveReport) {
        tracing::warn!(target: "regia_script_lua", code = error.code.code(), "mod state save refused: {}", error.message);
        for line in &mut report.mods {
            line.bytes = 0;
            line.error = Some(error.clone());
        }
        for saved in &file.mods {
            if report.mods.iter().all(|line| line.id != saved.id) {
                report.mods.push(ModSaveLine {
                    id: saved.id.clone(),
                    bytes: 0,
                    skipped_entities: 0,
                    error: Some(error.clone()),
                });
            }
        }
        if !report.mods.iter().any(|line| line.error.is_some()) {
            for id in self.held.keys() {
                report.mods.push(ModSaveLine {
                    id: id.clone(),
                    bytes: 0,
                    skipped_entities: 0,
                    error: Some(error.clone()),
                });
            }
        }
        report.mods.sort_by(|a, b| a.id.cmp(&b.id));
        report.kept.clear();
        report.bytes = 0;
        self.last_save = Some(report.clone());
        (Vec::new(), report)
    }

    /// Serializes every mod's state (format `luastate`, spec 84 § 11): runs each active mod's
    /// `on_save`, then reads `regia.state`, its random number generator and its entities. One
    /// mod's capture failure leaves out that mod. An `on_save` error disables the mod like any call and
    /// its state saves as it stood; state that is not plain data or is over the caps leaves the mod
    /// out. Both are reported on its line. A disabled mod saves what it has, or the entry it was
    /// restored from. Held entries take precedence over live captures of the same id. Their
    /// bytes are reserved first. A live mod that does not fit in what remains is left out with
    /// `lua_too_large` and the save continues. The save returns empty bytes only when the held
    /// entries alone pass the cap (or a capture cannot be measured). A restored entry whose id
    /// contains `:` is skipped with a warning and does not fail the save (ADR-7992).
    pub fn save_state(&mut self, world: &mut dyn ModWorld) -> (Vec<u8>, SaveReport) {
        let _span = tracing::info_span!("lua_mods.save").entered();
        let mut report = SaveReport::default();
        let mut file = LuaStateFile::default();
        let held_total = match self.held_encoded_total() {
            Ok(total) => total,
            Err(error) => return self.refuse_save(report, &file, error),
        };
        if held_total > self.limits.max_state_total_bytes {
            return self.refuse_save(
                report,
                &file,
                ModError::new(
                    LuaErrorCode::TooLarge,
                    format!(
                        "held entries alone are {held_total} bytes, past the {} a save holds",
                        self.limits.max_state_total_bytes
                    ),
                ),
            );
        }
        let held_ids: std::collections::BTreeSet<String> = self.held.keys().cloned().collect();
        let mut total = held_total;
        for module in &mut self.mods {
            let id = module.id().to_owned();
            let mut line = ModSaveLine {
                id: id.clone(),
                bytes: 0,
                skipped_entities: 0,
                error: None,
            };
            let hook = if module.is_active() {
                module.call_global(world, "on_save", ())
            } else {
                Ok(())
            };
            self.emitted.hold(module.take_outbox());
            // A running mod's own state wins over an old entry. A disabled mod that failed its
            // restore keeps its entry, through every save, until it runs again.
            let keep_previous = if module.is_active() {
                self.kept.remove(&id);
                None
            } else {
                self.kept.get(&id).cloned()
            };
            // A held entry replaces this capture. Its bytes are already in `total`.
            if held_ids.contains(&id) {
                line.error = hook.err();
                report.mods.push(line);
                continue;
            }
            // A failed `on_save` disabled the mod like any call; its state still saves as it stood,
            // as a disabled mod's does. A bug in the hook must not cost the player the mod's data.
            let captured = match keep_previous {
                Some(saved) => Ok((saved, 0)),
                None => module.capture(Some(world)),
            };
            let sized = captured.and_then(|(saved, skipped)| {
                let bytes = state::encoded_size(&saved)?;
                let next = total.saturating_add(bytes);
                if next > self.limits.max_state_total_bytes {
                    return Err(ModError::new(
                        LuaErrorCode::TooLarge,
                        format!(
                            "the mods' states pass the {} bytes a save holds",
                            self.limits.max_state_total_bytes
                        ),
                    ));
                }
                total = next;
                Ok((saved, skipped, bytes))
            });
            match sized {
                Ok((saved, skipped, bytes)) => {
                    line.bytes = bytes;
                    line.skipped_entities = skipped;
                    line.error = hook.err();
                    file.mods.push(saved);
                }
                Err(error) => {
                    tracing::warn!(target: "regia_script_lua", id, code = error.code.code(), "mod state left out of the save: {}", error.message);
                    if module.is_active() {
                        module.note_error("save", error.clone());
                    }
                    line.error = Some(error);
                }
            }
            report.mods.push(line);
        }
        for (id, saved) in &self.kept {
            if self.index(id).is_some() || id.contains(':') || self.held.contains_key(id) {
                continue;
            }
            let bytes = state::encoded_size(saved).unwrap_or(u64::MAX);
            if total.saturating_add(bytes) <= self.limits.max_state_total_bytes {
                total += bytes;
                report.kept.push(id.clone());
                file.mods.push(saved.clone());
            }
        }
        // Frozen v1 keeps manifest ids; held entries take precedence (ADR-7951, ADR-7992).
        let skipped = match self.bind_saved_keys(&mut file) {
            Ok(skipped) => skipped,
            Err(error) => return self.refuse_save(report, &file, error),
        };
        for id in skipped {
            report.mods.push(ModSaveLine {
                id,
                bytes: 0,
                skipped_entities: 0,
                error: Some(ModError::new(
                    LuaErrorCode::State,
                    "skipped a restored entry whose id contains ':'; it is not a v1 manifest id",
                )),
            });
        }
        for saved in self.held.values() {
            if let Some(line) = report.mods.iter_mut().find(|line| line.id == saved.id) {
                line.bytes = state::encoded_size(saved).unwrap_or(0);
            } else {
                report.kept.push(saved.id.clone());
            }
        }
        file.mods.sort_by(|a, b| a.id.cmp(&b.id));
        let bytes = match state::encode(&file) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(target: "regia_script_lua", "mod state left out of the save: {}", error.message);
                Vec::new()
            }
        };
        report.bytes = bytes.len() as u64;
        self.last_save = Some(report.clone());
        (bytes, report)
    }

    /// Rebuilds every mod from a save (`None`: a save with no mod state). Each mod runs its entry
    /// chunk in a fresh state. With an entry, `regia.state` and the random numbers are restored
    /// (through `on_migrate` when `regia.state_version` changed); a mod that defines `on_load`
    /// gets its saved entities back and that call, one that does not restarts with `on_start`.
    /// Without an entry the mod starts fresh. Its entities from before the load go either way.
    ///
    /// Component names are global, so a revived mod that re-declares a name another mod holds is
    /// refused like a load (`lua_bad_value`): the declaration is not registered, the mod stays
    /// disabled, its entities go and its saved entry is kept.
    ///
    /// Events queued or emitted before the load are dropped: they belong to the abandoned
    /// timeline. A failing mod is disabled and its entry kept, as is the entry of a mod that is
    /// not running, so the next save does not lose them.
    pub fn restore_state(&mut self, bytes: Option<&[u8]>, world: &mut dyn ModWorld) -> RestoreReport {
        let _span = tracing::info_span!("lua_mods.restore").entered();
        let mut report = RestoreReport::default();
        let file = match bytes.map(|bytes| state::decode(bytes, &self.limits)) {
            Some(Ok(file)) => file,
            Some(Err(error)) => {
                tracing::warn!(target: "regia_script_lua", code = error.code.code(), "saved mod state refused: {}", error.message);
                report.error = Some(error);
                LuaStateFile::default()
            }
            None => LuaStateFile::default(),
        };
        // V1 manifest entries cannot prove ownership by a Workshop mount (ADR-7951).
        let file = self.bind_restored_keys(file);
        self.queued.clear();
        self.emitted.clear();
        self.kept.clear();
        let mut entries: BTreeMap<String, ModState> =
            file.mods.into_iter().map(|m| (m.id.clone(), m)).collect();
        for index in 0..self.mods.len() {
            let old = &self.mods[index];
            let id = old.id().to_owned();
            let line = match LuaMod::build(
                old.manifest.clone(),
                Arc::clone(&old.files),
                &self.limits,
                self.seed,
                &self.side,
            ) {
                Ok(mut fresh) => {
                    fresh.inherit(old);
                    let entry = entries.remove(&id);
                    let owned = world.owned(&id);
                    let revived = match &entry {
                        Some(saved) => saved
                            .check(&self.limits)
                            .and_then(|()| fresh.revive(world, Some(saved), &owned)),
                        None => fresh.revive(world, None, &owned),
                    };
                    // Component names are global (spec 84 § 14): a revived mod that re-declares
                    // a name another mod holds — one already rebuilt above, or one still holding
                    // it below — is refused like a load. Its declaration is not registered and,
                    // like a failed revive, its entities go and its saved entry is kept.
                    let revived = match revived {
                        Ok(carry) => match component_name_clash(&self.mods, &fresh, Some(id.as_str())) {
                            Some(error) => {
                                fresh.fail("restore", error.clone());
                                fresh.clear_components();
                                Err(error)
                            }
                            None => Ok(carry),
                        },
                        Err(error) => Err(error),
                    };
                    if revived.is_ok() {
                        reconcile_components(&fresh, world);
                    }
                    if let Err(error) = &revived
                        && fresh.is_active()
                    {
                        fresh.fail("restore", error.clone());
                    }
                    // The entities the mod owned before the load go either way (spec 84 § 11),
                    // and so does what a failed revive spawned before it stopped: the mod is
                    // disabled and its saved entry kept (audit wave 5, luasave H1).
                    if revived.is_err() {
                        for entity in world.owned(&id) {
                            world.despawn(&id, entity);
                        }
                    }
                    // A corrupt entry (a NaN or doubled key, a dangling table) can never load:
                    // keeping it would write it into every later save and disable the mod on
                    // every load (audit wave 6, luasave H2). The mod saves what it has instead.
                    if let (Err(_), Some(saved)) = (&revived, entry)
                        && !saved.is_corrupt()
                    {
                        self.kept.insert(id.clone(), saved);
                    }
                    self.emitted.hold(fresh.take_outbox());
                    self.mods[index] = fresh;
                    ModRestoreLine {
                        id,
                        state: revived.as_ref().ok().copied(),
                        error: revived.err(),
                    }
                }
                Err(error) => {
                    // The entities from before the load go either way (spec 84 § 11).
                    for entity in world.owned(&id) {
                        world.despawn(&id, entity);
                    }
                    self.mods[index].fail("restore", error.clone());
                    ModRestoreLine {
                        id,
                        state: None,
                        error: Some(error),
                    }
                }
            };
            report.mods.push(line);
        }
        report.kept = entries.keys().cloned().collect();
        self.kept.extend(entries);
        self.last_restore = Some(report.clone());
        report
    }

    /// The report of the last [`save_state`](Self::save_state).
    #[must_use]
    pub fn last_save(&self) -> Option<&SaveReport> {
        self.last_save.as_ref()
    }

    /// The report of the last [`restore_state`](Self::restore_state).
    #[must_use]
    pub fn last_restore(&self) -> Option<&RestoreReport> {
        self.last_restore.as_ref()
    }

    /// Saved entries with no running mod, by id.
    pub fn kept_state(&self) -> impl Iterator<Item = &str> {
        self.kept.keys().map(String::as_str)
    }

    /// What mod `id` would save now, read without running any of its code (no `on_save`).
    ///
    /// # Errors
    /// `lua_unknown_mod`, or why the state could not be saved.
    pub fn peek_state(&self, id: &str, world: &mut dyn ModWorld) -> Result<ModState, ModError> {
        self.get(id)
            .ok_or_else(|| unknown(id))?
            .capture(Some(world))
            .map(|(state, _)| state)
    }

    /// Mod `id`.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&LuaMod> {
        self.index(id).map(|i| &self.mods[i])
    }

    /// Every mod, in id order.
    pub fn iter(&self) -> impl Iterator<Item = &LuaMod> {
        self.mods.iter()
    }

    /// Queues game event `name` for every subscribed mod; delivered by
    /// [`deliver_events`](Self::deliver_events) (the plugin does it after each fixed step).
    pub fn fire(&mut self, name: impl Into<String>, value: ScriptValue) {
        self.queued.push((name.into(), value));
    }

    /// Delivers the queued events, in queue order: each to every active mod in id order, each
    /// mod's handlers in subscription order. One pass is one call as far as the budgets go: all of
    /// a mod's handlers share its instruction budget and its emit and spawn caps, so a mod cannot
    /// multiply its budget by subscribing many handlers.
    pub fn deliver_events(&mut self, world: &mut dyn ModWorld) {
        let queued = std::mem::take(&mut self.queued);
        if queued.is_empty() {
            return;
        }
        let _span = tracing::info_span!("lua_mods.events", count = queued.len()).entered();
        for module in &mut self.mods {
            module.begin_budget();
            module.delivering = true;
        }
        for (name, value) in &queued {
            for module in &mut self.mods {
                if !module.is_active() {
                    continue;
                }
                for handler in module.handlers(name) {
                    let value = value.clone();
                    if module
                        .call(world, name, move |lua| handler.call::<()>(value.to_lua(lua)?))
                        .is_err()
                    {
                        break;
                    }
                }
                self.emitted.hold(module.take_outbox());
            }
        }
        for module in &mut self.mods {
            module.delivering = false;
        }
    }

    /// The events mods emitted since the last drain, in emission order.
    ///
    /// ```
    /// use regia_script_lua::{LuaLimits, LuaMods, ModFiles, NoWorld};
    ///
    /// let manifest = r#"(format_version: 1, id: "demo", name: "Demo", version: "1.0.0", entry: "main.lua", api_version: 1)"#;
    /// let script = "function on_start() regia.emit('first') regia.emit('second') end";
    /// let mut mods = LuaMods::new(1, LuaLimits::default());
    /// mods.load(ModFiles::from_map([("luamod.ron", manifest), ("main.lua", script)]), &mut NoWorld).unwrap();
    ///
    /// let names: Vec<String> = mods.drain_emitted().into_iter().map(|e| e.name).collect();
    /// assert_eq!(names, ["first", "second"]);
    /// // Drained once: the queue is empty afterwards.
    /// assert!(mods.drain_emitted().is_empty());
    /// ```
    pub fn drain_emitted(&mut self) -> Vec<EmittedEvent> {
        self.emitted.take()
    }

    /// Events waiting for [`drain_emitted`](Self::drain_emitted).
    #[must_use]
    pub fn emitted_held(&self) -> usize {
        self.emitted.events.len()
    }

    /// Events lost, in this host's life, because nothing drained them in time
    /// ([`MAX_EMITTED_HELD`], [`MAX_EMITTED_HELD_BYTES`]).
    #[must_use]
    pub fn emitted_dropped(&self) -> u64 {
        self.emitted.dropped
    }

    /// This host on `authority`'s side (ADR-8466). Set it before loading mods: it applies to every
    /// state built after the call (loads, restarts, reloads, `lua.run`'s scratch state).
    ///
    /// ```
    /// use regia_script_lua::{LuaLimits, LuaMods, ModFiles, NoWorld, ScriptAuthority};
    ///
    /// let manifest = r#"(format_version: 1, id: "hud", name: "Hud", version: "1", entry: "main.lua", api_version: 1)"#;
    /// let main = "assert(regia.command == nil and regia.authority == 'presentation')";
    /// let mut mods = LuaMods::new(1, LuaLimits::default()).with_authority(ScriptAuthority::Presentation);
    /// mods.load(ModFiles::from_map([("luamod.ron", manifest), ("main.lua", main)]), &mut NoWorld)
    ///     .expect("a client mod has no regia.command");
    /// ```
    #[must_use]
    pub fn with_authority(mut self, authority: ScriptAuthority) -> Self {
        self.side.authority = authority;
        self
    }

    /// This host with `rules` for `regia.command` (see [`Self::set_host_commands`]).
    #[must_use]
    pub fn with_host_commands(mut self, rules: HostCommandRules) -> Self {
        self.set_host_commands(rules);
        self
    }

    /// Sets which command names `regia.command` accepts and the queue caps, for every mod now
    /// and later. Commands already waiting stay, even past a lower cap.
    pub fn set_host_commands(&mut self, rules: HostCommandRules) {
        self.side.rules = Arc::new(rules);
        for module in &self.mods {
            let rules = Arc::clone(&self.side.rules);
            module.write_authority(move |a| a.rules = rules);
        }
    }

    /// The side this host runs on.
    #[must_use]
    pub fn authority(&self) -> ScriptAuthority {
        self.side.authority
    }

    /// What `regia.command` accepts.
    #[must_use]
    pub fn host_command_rules(&self) -> &HostCommandRules {
        &self.side.rules
    }

    /// Takes every command the mods queued with `regia.command`: in mod id order, then in the
    /// order each mod queued them. The game applies each through its own command path (and may
    /// still refuse it); the engine never acts on one. Empty on a presentation host.
    ///
    /// ```
    /// use regia_script_lua::{HostCommandRules, LuaLimits, LuaMods, ModFiles, NoWorld};
    ///
    /// let manifest = r#"(format_version: 1, id: "ai", name: "Ai", version: "1", entry: "main.lua", api_version: 1)"#;
    /// let main = "function on_tick(dt) local ok, why = regia.command('order', { unit = 3 }) end";
    /// let mut mods = LuaMods::new(1, LuaLimits::default()).with_host_commands(HostCommandRules::new(["order"]));
    /// mods.load(ModFiles::from_map([("luamod.ron", manifest), ("main.lua", main)]), &mut NoWorld)
    ///     .expect("loads");
    /// mods.call_tick(&mut NoWorld, 0.1);
    /// let commands = mods.drain_host_commands();
    /// assert_eq!(commands.len(), 1);
    /// assert_eq!((commands[0].mod_id.as_str(), commands[0].name.as_str()), ("ai", "order"));
    /// ```
    pub fn drain_host_commands(&mut self) -> Vec<HostCommand> {
        let mut out = Vec::new();
        for module in &mut self.mods {
            out.extend(module.take_commands());
        }
        out
    }

    /// Commands waiting for [`Self::drain_host_commands`], over every mod.
    #[must_use]
    pub fn pending_host_commands(&self) -> usize {
        self.mods.iter().map(LuaMod::pending_host_commands).sum()
    }

    /// Up to `limit` waiting commands, in drain order, without taking them (the agent's view).
    #[must_use]
    pub fn peek_host_commands(&self, limit: usize) -> Vec<HostCommand> {
        let mut out = Vec::new();
        for module in &self.mods {
            if out.len() >= limit {
                break;
            }
            let room = limit - out.len();
            if let Some(some) =
                module.read_authority(|a| a.pending.iter().take(room).cloned().collect::<Vec<_>>())
            {
                out.extend(some);
            }
        }
        out
    }

    /// Publishes what scripts may read: every mod's `regia.snapshot()` returns it from now on (as
    /// that mod's own table, built at most once per snapshot), and every active mod that defines
    /// `on_snapshot(snapshot)` is called with it, in id order, under its budget (the build is
    /// charged to that call). A snapshot equal to the last one changes nothing: no hook runs, no
    /// table is rebuilt, and the result is `Ok(false)`. A hook error disables that mod only.
    ///
    /// A mod's writes to its table stay in that mod until the next snapshot; the host never reads
    /// them back. Both sides get snapshots.
    ///
    /// # Errors
    /// `lua_too_large` past [`MAX_SNAPSHOT_BYTES`]; `lua_bad_value` for a non-finite number in
    /// it. Nothing is published then.
    pub fn publish_snapshot(
        &mut self,
        world: &mut dyn ModWorld,
        snapshot: ScriptValue,
    ) -> Result<bool, ModError> {
        let _span = tracing::debug_span!("lua_mods.publish_snapshot").entered();
        let weight = snapshot.weight();
        if weight > MAX_SNAPSHOT_BYTES {
            return Err(ModError::new(
                LuaErrorCode::TooLarge,
                format!("a snapshot of about {weight} bytes is over the {MAX_SNAPSHOT_BYTES} allowed"),
            ));
        }
        if !authority::all_finite(&snapshot) {
            return Err(ModError::new(
                LuaErrorCode::BadValue,
                "a snapshot holds a number that is not finite",
            ));
        }
        if self.side.snapshot.as_deref() == Some(&snapshot) {
            return Ok(false);
        }
        let snapshot = Arc::new(snapshot);
        self.side.generation += 1;
        self.side.snapshot = Some(Arc::clone(&snapshot));
        self.side.weight = weight;
        let generation = self.side.generation;
        for module in &mut self.mods {
            let snapshot = Arc::clone(&snapshot);
            module.write_authority(move |a| {
                a.snapshot = Some(snapshot);
                a.generation = generation;
                a.weight = weight;
            });
            if !module.is_active() {
                continue;
            }
            let Some(hook) = module.global_fn("on_snapshot") else {
                continue;
            };
            // An error is recorded on the mod and disables it; the others carry on.
            let _ = module.call(world, "on_snapshot", move |lua| {
                let table = authority::snapshot_table(lua)?;
                hook.call::<()>(table)
            });
            self.emitted.hold(module.take_outbox());
        }
        Ok(true)
    }

    /// The last published snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Option<&ScriptValue> {
        self.side.snapshot.as_deref()
    }

    /// How many changed snapshots were published (0: none yet).
    #[must_use]
    pub fn snapshot_generation(&self) -> u64 {
        self.side.generation
    }

    /// Calls every active mod's `on_update(dt)`, in id order, then every declared component's
    /// per-entity `on_update(entity, dt)` hook.
    pub fn call_update(&mut self, world: &mut dyn ModWorld, dt: f64) {
        let _span = tracing::info_span!("lua_mods.update").entered();
        self.call_all(world, "on_update", dt);
        for module in &mut self.mods {
            if module.is_active() {
                module.update_components(world, dt);
                self.emitted.hold(module.take_outbox());
            }
        }
    }

    /// Calls every active mod's `on_tick(dt)`, in id order, then delivers the queued events.
    pub fn call_tick(&mut self, world: &mut dyn ModWorld, dt: f64) {
        let _span = tracing::info_span!("lua_mods.tick").entered();
        self.call_all(world, "on_tick", dt);
        self.deliver_events(world);
    }

    fn call_all(&mut self, world: &mut dyn ModWorld, name: &'static str, dt: f64) {
        for module in &mut self.mods {
            if module.is_active() {
                // An error is recorded on the mod and disables it; the others carry on.
                let _ = module.call_global(world, name, dt);
                self.emitted.hold(module.take_outbox());
            }
        }
    }

    /// Calls global function `function` of mod `id` with `arg` and returns its first result (`nil`
    /// when it returns nothing). This is the logic graph's `logic::CallLua` action (spec 102 § 4.4):
    /// the call runs under the mod's instruction budget and memory cap like a callback, and what it
    /// emits is held for [`drain_emitted`](Self::drain_emitted); unlike a callback, an error (a Lua
    /// error, the budget, no such function) is only returned, and the mod stays enabled.
    pub fn call_function(
        &mut self,
        world: &mut dyn ModWorld,
        id: &str,
        function: &str,
        arg: ScriptValue,
    ) -> Result<ScriptValue, ModError> {
        let index = self.index(id).ok_or_else(|| unknown(id))?;
        let module = &mut self.mods[index];
        if let ModStatus::Disabled { code, message } = &module.status {
            return Err(ModError::new(
                LuaErrorCode::Disabled,
                format!("the mod is disabled ({code}: {message})"),
            ));
        }
        let Some(f) = module.global_fn(function) else {
            return Err(ModError::new(
                LuaErrorCode::Runtime,
                format!("mod {id} has no global function {function:?}"),
            ));
        };
        let result = module.run_call(world, function, 0, move |lua| {
            let out = f.call::<Value>(arg.to_lua(lua)?)?;
            ScriptValue::from_lua(&out)
        });
        self.emitted.hold(module.take_outbox());
        result
    }

    /// Runs `chunk` inside mod `id`'s state, or in a throwaway sandbox with the host limits when
    /// `id` is `None`. In a mod, an error disables it like any other call; a call the host
    /// refuses because the mod is already disabled never ran, so it reports zero instructions
    /// and zero bytes. The sandbox's spawns go with it: nothing a scratch call leaves persists
    /// in the world (a persistent setup belongs to a mod).
    pub fn run_chunk(
        &mut self,
        world: &mut dyn ModWorld,
        id: Option<&str>,
        chunk: &str,
    ) -> Result<RunOutcome, ModError> {
        if chunk.len() > MAX_CHUNK_BYTES {
            return Err(ModError::new(
                LuaErrorCode::TooLarge,
                format!("a chunk is at most {MAX_CHUNK_BYTES} bytes"),
            ));
        }
        let mut scratch;
        let scratch_run = id.is_none();
        let module = match id {
            Some(id) => {
                let index = self.index(id).ok_or_else(|| unknown(id))?;
                &mut self.mods[index]
            }
            None => {
                let manifest = LuaModManifest {
                    format_version: crate::LUAMOD_FORMAT_VERSION,
                    id: SCRATCH_ID.to_owned(),
                    name: "agent scratch".to_owned(),
                    version: String::new(),
                    entry: "main.lua".to_owned(),
                    api_version: crate::API_VERSION,
                    instruction_budget: None,
                    memory_limit_bytes: None,
                };
                scratch = LuaMod::build(
                    manifest,
                    Arc::new(ModFiles::default()),
                    &self.limits,
                    self.seed,
                    &self.side,
                )?;
                &mut scratch
            }
        };
        let chunk = chunk.to_owned();
        let result = module.call(world, "lua.run", move |lua| {
            let values = sandbox::load_text(lua, &chunk, "lua.run")?.call::<MultiValue>(())?;
            // One value is capped; a chunk can return thousands of them. They share one allowance:
            // a value that would take the results past it is described instead.
            let mut bytes = 0u64;
            Ok(values
                .iter()
                .map(|v| {
                    let described = || ScriptValue::String(format!("<{}>", lua_type(v)));
                    if bytes >= MAX_VALUE_BYTES as u64 {
                        return described();
                    }
                    match ScriptValue::from_lua(v) {
                        Ok(value) => {
                            bytes = bytes.saturating_add(value.weight());
                            if bytes > MAX_VALUE_BYTES as u64 {
                                return described();
                            }
                            value
                        }
                        Err(_) => described(),
                    }
                })
                .collect::<Vec<_>>())
        });
        if scratch_run {
            // The throwaway sandbox takes its spawns with it, or a probing session silently
            // grows the world until the entity cap refuses every later scratch spawn.
            for entity in world.owned(SCRATCH_ID) {
                world.despawn(SCRATCH_ID, entity);
            }
        }
        let log = module
            .lua
            .app_data_ref::<ModRuntime>()
            .map(|rt| rt.call_log.clone())
            .unwrap_or_default();
        let counters = module.counters;
        self.emitted.hold(module.take_outbox());
        Ok(match result {
            Ok(results) => RunOutcome {
                ok: true,
                results,
                error: None,
                instructions: counters.last_instructions,
                memory_bytes: counters.memory_bytes,
                log,
            },
            Err(error) => {
                // A call refused because the mod was already disabled never ran: its counters
                // are the last real call's, not this chunk's, so they read as zero.
                let refused = error.code == LuaErrorCode::Disabled;
                RunOutcome {
                    ok: false,
                    results: Vec::new(),
                    error: Some(error),
                    instructions: if refused { 0 } else { counters.last_instructions },
                    memory_bytes: if refused { 0 } else { counters.memory_bytes },
                    log,
                }
            }
        })
    }

    /// The globals of mod `id` (or the fields of the table at dotted `path`), sorted by key, at
    /// most `limit` entries. Reads raw fields only: no metamethod and no Lua code runs.
    pub fn globals(&self, id: &str, path: Option<&str>, limit: usize) -> Result<Vec<GlobalEntry>, ModError> {
        let module = self.get(id).ok_or_else(|| unknown(id))?;
        let mut table = module.lua.globals();
        if let Some(path) = path.filter(|p| !p.is_empty()) {
            for segment in path.split('.') {
                match table.raw_get::<Value>(segment) {
                    Ok(Value::Table(t)) => table = t,
                    _ => {
                        return Err(ModError::new(
                            LuaErrorCode::BadValue,
                            format!("`{path}` is not a table in mod `{id}`"),
                        ));
                    }
                }
            }
        }
        // The keys first: strings are interned, so a table of references to one long string would
        // cost its length per entry on the Rust heap, for entries past `limit` as well.
        let key_text = |key: &Value| match key {
            Value::String(s) => String::from_utf8_lossy(&s.as_bytes()).into_owned(),
            Value::Integer(i) => i.to_string(),
            Value::Number(n) => n.to_string(),
            other => format!("<{}>", lua_type(other)),
        };
        // Only the `limit` smallest keys are held, so a table of millions costs `limit` keys
        // (ADR-1957); values are read in a second pass, for those keys only.
        let mut wanted: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (k, _) in table.pairs::<Value, Value>().filter_map(Result::ok) {
            let key = key_text(&k);
            if wanted.len() >= limit && wanted.last().is_none_or(|last| key >= *last) {
                continue;
            }
            wanted.insert(key);
            if wanted.len() > limit {
                wanted.pop_last();
            }
        }
        // The number key `1` and the string key `"1"` share one key text; the second pair a key
        // text already listed is skipped, so a key is never listed twice and `entries.len()`
        // keeps the distinct-key promise the limit makes.
        let mut listed: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut entries: Vec<GlobalEntry> = table
            .pairs::<Value, Value>()
            .filter_map(Result::ok)
            .filter_map(|(k, v)| {
                let key = key_text(&k);
                if !wanted.contains(&key) || !listed.insert(key.clone()) {
                    return None;
                }
                let scalar = matches!(
                    v,
                    Value::Nil
                        | Value::Boolean(_)
                        | Value::Integer(_)
                        | Value::Number(_)
                        | Value::String(_)
                        | Value::UserData(_)
                );
                Some(GlobalEntry {
                    key,
                    kind: lua_type(&v).to_owned(),
                    value: if scalar {
                        ScriptValue::from_lua(&v).ok()
                    } else {
                        None
                    },
                    len: match &v {
                        Value::Table(t) => Some(t.raw_len()),
                        _ => None,
                    },
                })
            })
            .collect();
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        entries.truncate(limit);
        Ok(entries)
    }

    fn index(&self, id: &str) -> Option<usize> {
        self.mods.binary_search_by(|m| m.id().cmp(id)).ok()
    }
}

/// A component name `module` shares with another loaded mod, when names are global.
fn component_name_clash(mods: &[LuaMod], module: &LuaMod, except: Option<&str>) -> Option<ModError> {
    let (name, owner) = declared_names(mods, except)
        .into_iter()
        .find(|(name, _)| module.components().iter().any(|decl| decl.name == *name))?;
    Some(ModError::new(
        LuaErrorCode::BadValue,
        format!("component `{name}` is already declared by mod `{owner}`"),
    ))
}

/// The component declaration names of every mod, optionally excluding `except` (a reload keeps
/// its own mod's names), mapped to the declaring mod's id.
fn declared_names(mods: &[LuaMod], except: Option<&str>) -> std::collections::BTreeMap<String, String> {
    let mut names = std::collections::BTreeMap::new();
    for module in mods {
        if except.is_some_and(|id| module.id() == id) {
            continue;
        }
        for decl in module.components() {
            names.insert(decl.name, module.id().to_owned());
        }
    }
    names
}

/// Reconciles every entity carrying one of `module`'s components against the current
/// declarations: kept values survive a reload, new properties get their defaults, removed ones go
/// (ADR-3356). Runs through the world seam so a trial reload's journal records the writes.
fn reconcile_components(module: &LuaMod, world: &mut dyn ModWorld) {
    for decl in module.components() {
        for entity in world.entities_with(&decl.name) {
            let Some(props) = world.component_props(entity, &decl.name) else {
                continue;
            };
            let stored: std::collections::BTreeMap<String, LuaValue> = props
                .into_iter()
                .map(|(name, value)| (name, LuaValue::from_script(&value)))
                .collect();
            let merged = decl.reconcile(&stored);
            let values: Vec<(String, ScriptValue)> = merged
                .into_iter()
                .map(|(name, value)| (name, value.to_script()))
                .collect();
            if world.set_component_props(entity, &decl.name, values) {
                tracing::debug!(target: "regia_script_lua", id = %module.id(), component = %decl.name, entity, "component values reconciled");
            }
        }
    }
}

/// The host-side inputs of one reload: the limits the new version is built under and the host seed.
struct ReloadSeed<'a> {
    limits: &'a LuaLimits,
    seed: u64,
    side: &'a HostSide,
}

/// Builds the new version of `old` from `files` and moves the mod onto it. `kept` is a saved entry
/// the old version could not take (its restore failed): the new version takes it instead of what
/// the disabled version holds. `reserved` names the component declarations of the other mods: a
/// new version that re-declares one of them is refused before anything of its world writes
/// becomes final. On failure the world is as it was and `outcome.stage` says where it stopped.
fn replace_mod(
    old: &mut LuaMod,
    files: Option<ModFiles>,
    kept: Option<&ModState>,
    reserved: &std::collections::BTreeMap<String, String>,
    host: &ReloadSeed<'_>,
    world: &mut dyn ModWorld,
    outcome: &mut ReloadOutcome,
) -> Result<LuaMod, ModError> {
    let files = match files {
        Some(files) => files,
        None => {
            let Some(dir) = old.files.source_dir() else {
                return Err(ModError::new(
                    LuaErrorCode::Io,
                    "the mod was not loaded from a directory: pass its new files",
                ));
            };
            ModFiles::from_dir(dir, host.limits.max_mod_bytes)?
        }
    };
    let manifest = check_files(&files, host.limits)?;
    if manifest.id != old.manifest.id {
        return Err(ModError::new(
            LuaErrorCode::Manifest,
            format!(
                "the new files name mod `{}`, not `{}`",
                manifest.id, old.manifest.id
            ),
        ));
    }
    if files.get(&manifest.entry).is_none() {
        return Err(ModError::new(
            LuaErrorCode::Manifest,
            format!("entry `{}` is not in the mod", manifest.entry),
        ));
    }
    let mut fresh = LuaMod::build(manifest, Arc::new(files), host.limits, host.seed, host.side)?;
    fresh.apply_entity_cap(old.entity_cap);
    fresh.trial = true;

    outcome.stage = ReloadStage::Compile;
    fresh.compile_all()?;

    let mut journal = JournalWorld::new(world, &old.manifest.id);
    let rng_before = old.rng();
    // The old version's `on_save` runs against the journal, so it may copy values of world writes
    // the rollback undoes into its `regia.state`. Its state is taken first and put back on a
    // rollback, with the RNG (audit wave 5, luasave M2).
    let mut state_before: Option<StateTree> = None;
    let result = (|| {
        let owned = journal.owned(&old.manifest.id);
        outcome.stage = ReloadStage::Entry;
        // The old version's entities are taken over or go at the start stage: until then they do
        // not count against the new version's entity cap (ADR-2418).
        fresh.exclude_from_cap(Some(owned.as_slice()));
        fresh.run_entry(&mut journal)?;

        // A component name another mod declares is refused here, before anything the new entry
        // chunk wrote to the world becomes final (the journal rolls the writes back).
        for decl in fresh.components() {
            if let Some(owner) = reserved.get(&decl.name) {
                return Err(ModError::new(
                    LuaErrorCode::BadValue,
                    format!("component `{}` is already declared by mod `{owner}`", decl.name),
                ));
            }
        }

        // Only now does anything of the old version run: a new version that cannot even start
        // leaves it untouched. A failing `on_save` is not a failed reload, or the bug could only be
        // fixed by restarting the game: the state is read as it stands, as for a disabled mod.
        outcome.stage = ReloadStage::Snapshot;
        if old.is_active() && old.global_fn("on_save").is_some() {
            state_before = old.capture(None).ok().map(|(saved, _)| saved.tree);
        }
        if old.is_active()
            && let Some(on_save) = old.global_fn("on_save")
            && let Err(error) = old.run_call(&mut journal, "on_save", 0, move |_| on_save.call::<()>(()))
        {
            outcome.add_note(format!(
                "the old version's on_save failed, its state was read as it stood: {error}"
            ));
            old.note_error("on_save", error);
        }
        let captured;
        let usable = kept.filter(|saved| !old.is_active() && saved.check(host.limits).is_ok());
        let saved = match usable {
            Some(kept) => Some(kept),
            None => match old.capture(None) {
                Ok((saved, _)) => {
                    captured = saved;
                    Some(&captured)
                }
                Err(error) => {
                    outcome.add_note(error.to_string());
                    None
                }
            },
        };
        let from_version = old.state_version().ok();
        fresh.take_over(&mut journal, saved, &old.rng(), owned, from_version, outcome)?;
        // Stored per-entity values follow the new declaration before the journal commits, so a
        // later failure still rolls the reconcile back.
        reconcile_components(&fresh, &mut journal);
        fresh.trial = false;
        Ok(fresh)
    })();
    match result {
        Ok(fresh) => {
            journal.commit();
            Ok(fresh)
        }
        Err(error) => {
            journal.rollback();
            old.set_rng(&rng_before);
            if let Some(tree) = &state_before
                && let Err(restore) = old.set_state(tree)
            {
                tracing::warn!(target: "regia_script_lua", id = %old.manifest.id, "restoring the state after a failed reload: {restore}");
            }
            Err(error)
        }
    }
}

/// Reads and validates the manifest of `files` under the size cap.
fn check_files(files: &ModFiles, limits: &LuaLimits) -> Result<LuaModManifest, ModError> {
    if files.total_bytes() > limits.max_mod_bytes {
        return Err(ModError::new(
            LuaErrorCode::TooLarge,
            format!(
                "the mod's files are {} bytes, more than the {} allowed",
                files.total_bytes(),
                limits.max_mod_bytes
            ),
        ));
    }
    let Some(text) = files.get(LUAMOD_FILE) else {
        return Err(ModError::new(
            LuaErrorCode::Manifest,
            format!("the mod has no {LUAMOD_FILE}"),
        ));
    };
    LuaModManifest::parse(text).map_err(|e| ModError::new(LuaErrorCode::Manifest, e.to_string()))
}

fn unknown(id: &str) -> ModError {
    ModError::new(LuaErrorCode::UnknownMod, format!("no mod has id `{id}`"))
}

/// FNV-1a 64 of `id`: the mod's random stream.
fn stream_of(id: &str) -> u64 {
    id.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Appends a line to the running mod's log ring (and `tracing`).
pub(crate) fn push_log(lua: &Lua, line: &str) {
    let line = cap_message_to(line, MAX_LOG_LINE);
    if let Some(mut rt) = lua.app_data_mut::<ModRuntime>() {
        tracing::info!(target: "regia_script_lua", id = %rt.id, "{line}");
        if rt.log.len() >= rt.limits.max_log_lines.max(1) {
            rt.log.pop_front();
        }
        rt.log.push_back(line.clone());
        if rt.call_log.len() < rt.limits.max_log_lines.max(1) {
            rt.call_log.push(line);
        }
    }
}

fn cap_message_to(line: &str, max: usize) -> String {
    if line.len() <= max {
        return line.to_owned();
    }
    let mut end = max;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

/// World operations the `regia` table and entity handles send to the call's dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum WorldOp {
    Find = 0,
    Alive = 1,
    Name = 2,
    Position = 3,
    SetPosition = 4,
    Spawn = 5,
    Despawn = 6,
    HasComponent = 7,
    Attach = 8,
    RemoveComponent = 9,
    GetProps = 10,
    SetProps = 11,
}

impl WorldOp {
    fn from_u8(op: u8) -> Option<Self> {
        Some(match op {
            0 => WorldOp::Find,
            1 => WorldOp::Alive,
            2 => WorldOp::Name,
            3 => WorldOp::Position,
            4 => WorldOp::SetPosition,
            5 => WorldOp::Spawn,
            6 => WorldOp::Despawn,
            7 => WorldOp::HasComponent,
            8 => WorldOp::Attach,
            9 => WorldOp::RemoveComponent,
            10 => WorldOp::GetProps,
            11 => WorldOp::SetProps,
            _ => return None,
        })
    }
}

/// Sends `op` to the running call's world. Outside a call (no dispatcher) every lookup misses.
pub(crate) fn world_call(lua: &Lua, op: WorldOp, entity: u64, arg: Value) -> mlua::Result<Value> {
    world_call2(lua, op, entity, arg, Value::Nil)
}

pub(crate) fn world_call2(lua: &Lua, op: WorldOp, entity: u64, a: Value, b: Value) -> mlua::Result<Value> {
    match lua.named_registry_value::<Value>(HOST_KEY)? {
        Value::Function(host) => {
            let lo = (entity & 0xffff_ffff) as u32;
            let hi = (entity >> 32) as u32;
            host.call::<Value>((op as u8, lo, hi, a, b))
        }
        _ => Ok(match op {
            WorldOp::Alive
            | WorldOp::SetPosition
            | WorldOp::Despawn
            | WorldOp::HasComponent
            | WorldOp::Attach
            | WorldOp::RemoveComponent
            | WorldOp::SetProps => Value::Boolean(false),
            _ => Value::Nil,
        }),
    }
}

fn dispatch(
    lua: &Lua,
    world: &mut dyn ModWorld,
    owner: &str,
    op: u8,
    entity: u64,
    a: &Value,
    b: &Value,
) -> mlua::Result<Value> {
    let Some(op) = WorldOp::from_u8(op) else {
        return Ok(Value::Nil);
    };
    let budget = lua.app_data_ref::<ModRuntime>().map(|rt| Arc::clone(&rt.budget));
    if let Some(budget) = &budget {
        budget.charge(WORLD_OP_COST)?;
    }
    let handle = |bits: Option<u64>| -> mlua::Result<Value> {
        match bits {
            Some(bits) => crate::value::entity_value(lua, bits),
            None => Ok(Value::Nil),
        }
    };
    match op {
        WorldOp::Find => {
            let name = string_arg(a)?;
            // Charged before the lookup by the world's size, and after it by what it walked.
            let cost = FIND_BASE_COST + world.find_cost() / FIND_ENTITIES_PER_INSTRUCTION;
            if let Some(budget) = &budget {
                budget.charge(cost)?;
            }
            let found = world.find(&name);
            let scanned = world.take_scanned();
            if let Some(budget) = &budget {
                budget.charge(scanned)?;
            }
            handle(found)
        }
        WorldOp::Alive => Ok(Value::Boolean(world.alive(entity))),
        WorldOp::Name => match world.name(entity) {
            Some(n) => Ok(Value::String(lua.create_string(&n)?)),
            None => Ok(Value::Nil),
        },
        WorldOp::Position => match world.position(entity) {
            Some(p) => crate::value::counted_vec3(lua, p),
            None => Ok(Value::Nil),
        },
        WorldOp::SetPosition => {
            let p = vec3_arg(a)?;
            if !p.is_finite() {
                return Err(HostError::lua(LuaErrorCode::BadValue, "positions must be finite"));
            }
            Ok(Value::Boolean(world.set_position(entity, p)))
        }
        WorldOp::Spawn => {
            let name = string_arg(a)?;
            let p = match b {
                Value::Nil => Vec3::ZERO,
                other => vec3_arg(other)?,
            };
            if !p.is_finite() {
                return Err(HostError::lua(LuaErrorCode::BadValue, "positions must be finite"));
            }
            if let Some(mut rt) = lua.app_data_mut::<ModRuntime>() {
                if rt.spawns_this_call >= rt.limits.max_spawns_per_call {
                    return Err(HostError::lua(
                        LuaErrorCode::Runtime,
                        format!(
                            "a call may spawn at most {} entities",
                            rt.limits.max_spawns_per_call
                        ),
                    ));
                }
                // The entity cap (ADR-2418): what the game lets one mod own at once.
                // With nothing excluded, the count is enough. Listing every id is only
                // needed while a load or a reload is still holding the previous set.
                let (owned, counted) = if rt.cap_excluded.is_empty() {
                    let n = world.owned_count(owner);
                    (n, n)
                } else {
                    let owned_now = world.owned(owner);
                    let counted = owned_now
                        .iter()
                        .filter(|e| rt.cap_excluded.binary_search(e).is_err())
                        .count();
                    (owned_now.len(), counted)
                };
                if counted >= rt.entity_cap {
                    rt.entity_cap_refused += 1;
                    return Err(HostError::lua(
                        LuaErrorCode::EntityCap,
                        format!(
                            "mod `{owner}` may own at most {} entities (its entity cap); despawn some first",
                            rt.entity_cap
                        ),
                    ));
                }
                // A save keeps at most `max_state_entities` per mod and leaves the whole entry out
                // past it (audit v0.2.12: one legal spawn per tick crossed it in about a minute and
                // the mod's state was lost). Refuse the spawn here, where the script sees it.
                if owned >= rt.limits.max_state_entities {
                    return Err(HostError::lua(
                        LuaErrorCode::TooLarge,
                        format!(
                            "a mod may own at most {} entities (a save keeps no more); despawn some first",
                            rt.limits.max_state_entities
                        ),
                    ));
                }
                rt.spawns_this_call += 1;
            }
            handle(world.spawn(owner, &name, p))
        }
        WorldOp::Despawn => Ok(Value::Boolean(world.despawn(owner, entity))),
        WorldOp::HasComponent => {
            let decl = crate::value::component_arg(a)?;
            require_decl(lua, &decl)?;
            Ok(Value::Boolean(world.has_component(entity, &decl)))
        }
        WorldOp::Attach => {
            let decl_name = crate::value::component_arg(a)?;
            let initial = decl_initial(lua, &decl_name)?;
            Ok(Value::Boolean(
                world.attach_component(entity, &decl_name, initial),
            ))
        }
        WorldOp::RemoveComponent => {
            let decl = crate::value::component_arg(a)?;
            require_decl(lua, &decl)?;
            Ok(Value::Boolean(world.remove_component(entity, &decl)))
        }
        WorldOp::GetProps => {
            let decl = crate::value::component_arg(a)?;
            require_decl(lua, &decl)?;
            match world.component_props(entity, &decl) {
                Some(values) => ScriptValue::Map(values.into_iter().collect()).to_lua(lua),
                None => Ok(Value::Nil),
            }
        }
        WorldOp::SetProps => {
            let decl_name = crate::value::component_arg(a)?;
            let Value::Table(update) = b else {
                return Err(HostError::lua(
                    LuaErrorCode::BadValue,
                    "set_prop takes a property name and a value",
                ));
            };
            let key = crate::value::component_arg(&update.raw_get::<Value>(1)?)?;
            let prop = decl_prop(lua, &decl_name, &key)?;
            let value = ScriptValue::from_lua(&update.raw_get::<Value>(2)?)?;
            let coerced = prop
                .coerce(value)
                .map_err(|e| HostError::lua(LuaErrorCode::BadValue, e))?;
            Ok(Value::Boolean(
                world.set_component_prop(entity, &decl_name, &key, coerced),
            ))
        }
    }
}

/// The calling mod declared component `name`. Another mod's name is not in its registry.
fn require_decl(lua: &Lua, name: &str) -> mlua::Result<()> {
    let Some(rt) = lua.app_data_ref::<ModRuntime>() else {
        return Err(HostError::lua(LuaErrorCode::Runtime, "the host state is gone"));
    };
    if rt.decls.iter().any(|decl| decl.name == name) {
        Ok(())
    } else {
        Err(unknown_component(name))
    }
}

/// Initial values of the calling mod's component `name`.
fn decl_initial(lua: &Lua, name: &str) -> mlua::Result<Vec<(String, ScriptValue)>> {
    let Some(rt) = lua.app_data_ref::<ModRuntime>() else {
        return Err(HostError::lua(LuaErrorCode::Runtime, "the host state is gone"));
    };
    rt.decls
        .iter()
        .find(|decl| decl.name == name)
        .map(ComponentDecl::initial_values)
        .ok_or_else(|| unknown_component(name))
}

/// One property of the calling mod's component. Copies that declaration, not the entity's values.
fn decl_prop(lua: &Lua, decl_name: &str, key: &str) -> mlua::Result<crate::components::PropDecl> {
    let Some(rt) = lua.app_data_ref::<ModRuntime>() else {
        return Err(HostError::lua(LuaErrorCode::Runtime, "the host state is gone"));
    };
    let Some(decl) = rt.decls.iter().find(|decl| decl.name == decl_name) else {
        return Err(unknown_component(decl_name));
    };
    decl.prop(key).cloned().ok_or_else(|| {
        HostError::lua(
            LuaErrorCode::BadValue,
            format!("component `{decl_name}` has no property `{key}`"),
        )
    })
}

fn unknown_component(name: &str) -> mlua::Error {
    HostError::lua(
        LuaErrorCode::BadValue,
        format!("unknown component `{name}` (this mod declared no such component)"),
    )
}

fn string_arg(value: &Value) -> mlua::Result<String> {
    match value {
        Value::String(s) if s.as_bytes().len() <= 256 => {
            Ok(String::from_utf8_lossy(&s.as_bytes()).into_owned())
        }
        Value::String(_) => Err(HostError::lua(
            LuaErrorCode::BadValue,
            "names are at most 256 bytes",
        )),
        other => Err(HostError::lua(
            LuaErrorCode::BadValue,
            format!("expected a string, got {}", lua_type(other)),
        )),
    }
}

/// Lua 5.1 `math.random` semantics over PCG32: `()` in `[0, 1)`, `(m)` in `[1, m]`, `(m, n)` in
/// `[m, n]` (as `floor(r * (n - m + 1)) + m`).
fn random(rt: &mut ModRuntime, m: Option<f64>, n: Option<f64>) -> mlua::Result<f64> {
    let r = rt.rng.f64();
    let (lo, hi) = match (m, n) {
        (None, _) => return Ok(r),
        (Some(m), None) => (1.0, m.trunc()),
        (Some(m), Some(n)) => (m.trunc(), n.trunc()),
    };
    if !(lo.is_finite() && hi.is_finite() && (hi - lo + 1.0).is_finite()) || lo > hi {
        return Err(mlua::Error::runtime(
            "bad argument to 'random' (interval is empty)",
        ));
    }
    Ok((r * (hi - lo + 1.0)).floor() + lo)
}

/// Installs the `regia` table, `require`, `math.random`/`math.randomseed` and the per-state data.
fn install_api(
    lua: &Lua,
    id: &str,
    files: &Arc<ModFiles>,
    limits: &LuaLimits,
    seed: u64,
    budget: &Arc<Budget>,
    side: &HostSide,
) -> mlua::Result<()> {
    let stream = stream_of(id);
    let mut authority = ModAuthority::new(side.authority, Arc::clone(&side.rules));
    authority.snapshot = side.snapshot.clone();
    authority.generation = side.generation;
    authority.weight = side.weight;
    lua.set_app_data(ModRuntime {
        id: id.to_owned(),
        limits: *limits,
        stream,
        rng: Rng::with_stream(seed, stream),
        log: VecDeque::new(),
        call_log: Vec::new(),
        outbox: Vec::new(),
        emits_this_call: 0,
        emit_bytes_this_call: 0,
        spawns_this_call: 0,
        entity_cap: limits.max_entities_per_mod,
        entity_cap_refused: 0,
        cap_excluded: Vec::new(),
        decls: Vec::new(),
        budget: Arc::clone(budget),
        authority,
    });
    lua.set_named_registry_value(HANDLERS_KEY, lua.create_table()?)?;
    lua.set_named_registry_value(COMPONENTS_KEY, lua.create_table()?)?;
    lua.set_named_registry_value(LOADED_KEY, lua.create_table()?)?;
    lua.set_named_registry_value(LOADING_KEY, lua.create_table()?)?;

    let regia = lua.create_table()?;
    regia.raw_set("api_version", crate::API_VERSION)?;
    regia.raw_set("state", lua.create_table()?)?;
    lua.set_named_registry_value(REGIA_KEY, &regia)?;
    regia.raw_set(
        "on",
        lua.create_function(|lua, (name, handler): (mlua::LuaString, Function)| {
            let name = event_name(&name)?;
            let handlers: Table = lua.named_registry_value(HANDLERS_KEY)?;
            let list = match handlers.raw_get::<Value>(name.as_str())? {
                Value::Table(t) => t,
                _ => {
                    let t = lua.create_table()?;
                    handlers.raw_set(name.as_str(), &t)?;
                    t
                }
            };
            if list.raw_len() >= MAX_HANDLERS_PER_EVENT {
                return Err(HostError::lua(
                    LuaErrorCode::Runtime,
                    format!("a mod may subscribe at most {MAX_HANDLERS_PER_EVENT} handlers to one event"),
                ));
            }
            list.raw_push(handler)
        })?,
    )?;
    let declare_mod_id = id.to_owned();
    regia.raw_set(
        "component",
        lua.create_function(move |lua, (name, decl): (mlua::LuaString, Value)| {
            let name = String::from_utf8_lossy(&name.as_bytes()).into_owned();
            let Value::Table(table) = decl else {
                return Err(HostError::lua(
                    LuaErrorCode::BadValue,
                    format!("component `{name}`: the declaration is a table"),
                ));
            };
            // The cap before the parse: parsing is host work the budget does not see (ADR-5870).
            if lua
                .app_data_ref::<ModRuntime>()
                .is_some_and(|rt| rt.decls.len() >= MAX_DECLS_PER_MOD)
            {
                return Err(HostError::lua(
                    LuaErrorCode::BadValue,
                    format!("a mod declares at most {MAX_DECLS_PER_MOD} components"),
                ));
            }
            let parsed = parse_decl(&declare_mod_id, &name, &table)?;
            if let Some(mut rt) = lua.app_data_mut::<ModRuntime>() {
                if rt.decls.iter().any(|d| d.name == parsed.name) {
                    return Err(HostError::lua(
                        LuaErrorCode::BadValue,
                        format!("component `{}` is declared twice", parsed.name),
                    ));
                }
                rt.decls.push(parsed);
            }
            // Keep the declaration table (its on_update closure) rooted in the registry.
            let registry: Table = lua.named_registry_value(COMPONENTS_KEY)?;
            registry.raw_set(name.as_str(), table)?;
            Ok(())
        })?,
    )?;
    regia.raw_set(
        "emit",
        lua.create_function(|lua, (name, value, target): (mlua::LuaString, Value, Value)| {
            let name = event_name(&name)?;
            let value = ScriptValue::from_lua(&value)?;
            // The optional third argument names the entity the event is for (spec 102 § 4.4).
            let target = match target {
                Value::Nil => None,
                other => match ScriptValue::from_lua(&other)? {
                    ScriptValue::Entity(bits) => Some(bits),
                    _ => {
                        return Err(HostError::lua(
                            LuaErrorCode::BadValue,
                            "regia.emit: the target must be an entity".to_string(),
                        ));
                    }
                },
            };
            let Some(mut rt) = lua.app_data_mut::<ModRuntime>() else {
                return Ok(());
            };
            if rt.emits_this_call >= rt.limits.max_emits_per_call {
                return Err(HostError::lua(
                    LuaErrorCode::Runtime,
                    format!("a call may emit at most {} events", rt.limits.max_emits_per_call),
                ));
            }
            let weight = value.weight();
            if rt.emit_bytes_this_call + weight > rt.limits.max_emit_bytes_per_call {
                return Err(HostError::lua(
                    LuaErrorCode::BadValue,
                    format!(
                        "a call may emit at most {} bytes of event values",
                        rt.limits.max_emit_bytes_per_call
                    ),
                ));
            }
            // Only a queued event consumes the caps: a rejected emit leaves them alone, so the
            // script can catch the error and keep emitting what still fits.
            rt.emits_this_call += 1;
            rt.emit_bytes_this_call += weight;
            let mod_id = rt.id.clone();
            rt.outbox.push(EmittedEvent {
                mod_id,
                name,
                value,
                target,
            });
            Ok(())
        })?,
    )?;
    regia.raw_set(
        "log",
        lua.create_function(|lua, args: MultiValue| {
            // Only what the line keeps is copied: thousands of references to one long string
            // would otherwise take their whole length each on the Rust heap.
            let mut line = String::new();
            for (index, value) in args.iter().enumerate() {
                if line.len() > MAX_LOG_LINE {
                    break;
                }
                if index > 0 {
                    line.push('\t');
                }
                match value {
                    Value::String(s) => {
                        let bytes = s.as_bytes();
                        let kept = bytes.len().min(MAX_LOG_LINE + 4);
                        line.push_str(&String::from_utf8_lossy(&bytes[..kept]));
                    }
                    Value::Nil => line.push_str("nil"),
                    Value::Boolean(b) => line.push_str(&b.to_string()),
                    Value::Integer(i) => line.push_str(&i.to_string()),
                    Value::Number(n) => line.push_str(&n.to_string()),
                    other => line.push_str(&format!("<{}>", lua_type(other))),
                }
            }
            push_log(lua, &line);
            Ok(())
        })?,
    )?;
    regia.raw_set(
        "find",
        lua.create_function(|lua, name: Value| world_call(lua, WorldOp::Find, 0, name))?,
    )?;
    regia.raw_set(
        "spawn",
        lua.create_function(|lua, (name, position): (Value, Value)| {
            world_call2(lua, WorldOp::Spawn, 0, name, position)
        })?,
    )?;
    regia.raw_set(
        "vec3",
        lua.create_function(|lua, (x, y, z): (Option<f32>, Option<f32>, Option<f32>)| {
            crate::value::counted_vec3(
                lua,
                Vec3::new(x.unwrap_or(0.0), y.unwrap_or(0.0), z.unwrap_or(0.0)),
            )
        })?,
    )?;
    let random_fn = lua.create_function(|lua, args: mlua::Variadic<Value>| {
        // A nil argument is an error as in Lua 5.1; only a missing one means "no argument".
        let mut bounds = [None, None];
        if args.len() > 2 {
            return Err(mlua::Error::runtime("wrong number of arguments to 'random'"));
        }
        for (slot, arg) in bounds.iter_mut().zip(args.iter()) {
            *slot = Some(match arg {
                Value::Integer(i) => *i as f64,
                Value::Number(n) => *n,
                other => match lua.coerce_number(other.clone())? {
                    Some(n) => n,
                    None => {
                        return Err(mlua::Error::runtime(format!(
                            "bad argument to 'random' (number expected, got {})",
                            lua_type(other)
                        )));
                    }
                },
            });
        }
        let Some(mut rt) = lua.app_data_mut::<ModRuntime>() else {
            return Ok(0.0);
        };
        random(&mut rt, bounds[0], bounds[1])
    })?;
    regia.raw_set("random", &random_fn)?;
    // ADR-8466: the side this state runs on, the published snapshot, and (host only) the command
    // queue. A presentation state never gets `regia.command`: nothing it runs can reach the queue.
    regia.raw_set("authority", side.authority.name())?;
    regia.raw_set(
        "snapshot",
        lua.create_function(|lua, ()| authority::snapshot_table(lua))?,
    )?;
    if side.authority == ScriptAuthority::Host {
        regia.raw_set(
            "command",
            lua.create_function(|lua, (name, value): (mlua::LuaString, Value)| {
                let (outcome, cost, budget) = {
                    let Some(mut rt) = lua.app_data_mut::<ModRuntime>() else {
                        return Ok((false, Some("unknown_command")));
                    };
                    let rt = &mut *rt;
                    let (outcome, cost) = rt.authority.offer(&rt.id, &name.as_bytes(), &value);
                    (outcome, cost, rt.budget())
                };
                // The check is host work: it is charged like a world call, refused or not.
                budget.charge(cost)?;
                match outcome {
                    Ok(()) => Ok((true, None)),
                    Err(refusal) => Ok((false, Some(refusal.reason()))),
                }
            })?,
        )?;
    }
    let globals = lua.globals();
    globals.raw_set("regia", regia)?;

    let math: Table = globals.raw_get("math")?;
    math.raw_set("random", random_fn)?;
    math.raw_set(
        "randomseed",
        lua.create_function(|lua, s: f64| {
            if let Some(mut rt) = lua.app_data_mut::<ModRuntime>() {
                let seed = if s.is_finite() { s.trunc() as i64 as u64 } else { 0 };
                rt.rng = Rng::with_stream(seed, rt.stream);
            }
            Ok(())
        })?,
    )?;

    let files = Arc::clone(files);
    let mod_id = id.to_owned();
    globals.raw_set(
        "require",
        lua.create_function(move |lua, name: mlua::LuaString| require(lua, &files, &mod_id, &name))?,
    )?;
    Ok(())
}

fn event_name(name: &mlua::LuaString) -> mlua::Result<String> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_EVENT_NAME {
        return Err(HostError::lua(
            LuaErrorCode::BadValue,
            format!("event names are 1 to {MAX_EVENT_NAME} bytes"),
        ));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The file a module name names: `lib.util` and `lib/util` → `lib/util.lua`.
fn module_path(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".lua").unwrap_or(name);
    if stem.is_empty() || stem.starts_with('.') || stem.ends_with('.') {
        return None;
    }
    let path = format!("{}.lua", stem.replace('.', "/"));
    is_safe_relative(&path).then_some(path)
}

fn require(lua: &Lua, files: &ModFiles, mod_id: &str, name: &mlua::LuaString) -> mlua::Result<Value> {
    let name = String::from_utf8_lossy(&name.as_bytes()).into_owned();
    let Some(path) = module_path(&name) else {
        return Err(HostError::lua(
            LuaErrorCode::Require,
            format!("`{name}` is not a module name inside the mod"),
        ));
    };
    let loaded: Table = lua.named_registry_value(LOADED_KEY)?;
    let cached: Value = loaded.raw_get(path.as_str())?;
    // Lua 5.1 treats a false entry as not loaded, so a module that returned `false` runs again.
    if !matches!(cached, Value::Nil | Value::Boolean(false)) {
        return Ok(cached);
    }
    let loading: Table = lua.named_registry_value(LOADING_KEY)?;
    if loading.raw_get::<bool>(path.as_str()).unwrap_or(false) {
        return Err(HostError::lua(
            LuaErrorCode::Require,
            format!("`{name}` requires itself"),
        ));
    }
    let Some(source) = files.get(&path) else {
        return Err(HostError::lua(
            LuaErrorCode::Require,
            format!("module `{name}` ({path}) is not in the mod"),
        ));
    };
    let chunk = sandbox::load_text(lua, source, &format!("{mod_id}/{path}"))?;
    loading.raw_set(path.as_str(), true)?;
    let result = chunk.call::<Value>(name.as_str());
    loading.raw_set(path.as_str(), Value::Nil)?;
    let value = match result? {
        Value::Nil => Value::Boolean(true),
        v => v,
    };
    loaded.raw_set(path.as_str(), &value)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_is_a_send_sync_resource() {
        fn check<T: Send + Sync + 'static>() {}
        check::<LuaMods>();
    }

    #[test]
    fn module_names_map_inside_the_mod_only() {
        assert_eq!(module_path("lib.util").as_deref(), Some("lib/util.lua"));
        assert_eq!(module_path("lib/util").as_deref(), Some("lib/util.lua"));
        assert_eq!(module_path("main.lua").as_deref(), Some("main.lua"));
        for bad in ["", "..", "../x", "/abs", "a\\b", "a..b", ".hidden"] {
            assert_eq!(module_path(bad), None, "{bad}");
        }
    }

    #[test]
    fn streams_differ_per_id_and_are_stable() {
        assert_eq!(stream_of(""), 0xcbf2_9ce4_8422_2325);
        assert_ne!(stream_of("a"), stream_of("b"));
    }
}
