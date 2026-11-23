// SPDX-License-Identifier: MPL-2.0
//! One sandboxed Lua state per mod (ADR-1751 §§ 3-4): library whitelist, the `pcall` family
//! that re-raises budget errors, the instruction hook and the memory cap.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use mlua::{HookTriggers, Lua, LuaOptions, StdLib, Value, VmState};
use serde::{Deserialize, Serialize};

use crate::error::{LuaErrorCode, ModError};

/// Host budgets. A mod's manifest may ask for less, never more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LuaLimits {
    /// VM instructions one call may run (counted every `hook_interval` instructions).
    pub instruction_budget: u64,
    /// Bytes one state may allocate.
    pub memory_limit_bytes: u64,
    /// Instructions between two budget checks.
    pub hook_interval: u32,
    /// Bytes of all of a mod's files together.
    pub max_mod_bytes: u64,
    /// Lines kept in a mod's log ring.
    pub max_log_lines: usize,
    /// Entities one call may spawn.
    pub max_spawns_per_call: u32,
    /// Events one call may emit.
    pub max_emits_per_call: u32,
    /// Bytes one mod's saved state may take in a save (ADR-1943).
    pub max_state_bytes: u64,
    /// Tables plus entries one mod's `regia.state` may hold.
    pub max_state_items: usize,
    /// Entities one mod may have saved for it.
    pub max_state_entities: usize,
    /// Bytes every mod's saved state together may take in a save.
    pub max_state_total_bytes: u64,
    /// Bytes of event payloads one call may emit.
    pub max_emit_bytes_per_call: u64,
    /// The watchdog: milliseconds one call may run, checked with the instruction budget; 0 is off.
    /// A last resort for work the instruction count cannot see (one `..` of two huge strings is one
    /// instruction). A call that gets here has already missed every frame.
    pub max_call_millis: u32,
    /// The entity cap: entities one mod may own at once (spawned and not despawned). A spawn past
    /// it is refused with `lua_entity_cap` (ADR-2418). `LuaMods::set_entity_cap` can lower it for
    /// one mod. Absent in older serialized limits: the default.
    #[serde(default = "default_max_entities_per_mod")]
    pub max_entities_per_mod: usize,
}

/// The default entity cap (`LuaLimits::max_entities_per_mod`): 4,096, the most a save keeps.
pub const DEFAULT_MAX_ENTITIES_PER_MOD: usize = 4096;

fn default_max_entities_per_mod() -> usize {
    DEFAULT_MAX_ENTITIES_PER_MOD
}

impl Default for LuaLimits {
    fn default() -> Self {
        Self {
            instruction_budget: 1_000_000,
            memory_limit_bytes: 16 * 1024 * 1024,
            hook_interval: 1_000,
            max_mod_bytes: 4 * 1024 * 1024,
            max_log_lines: 256,
            max_spawns_per_call: 64,
            max_emits_per_call: 256,
            max_state_bytes: 1024 * 1024,
            max_state_items: 256 * 1024,
            max_state_entities: 4096,
            max_state_total_bytes: 8 * 1024 * 1024,
            max_emit_bytes_per_call: 1024 * 1024,
            max_call_millis: 1000,
            max_entities_per_mod: DEFAULT_MAX_ENTITIES_PER_MOD,
        }
    }
}

impl LuaLimits {
    /// These limits with a mod's requests applied (each clamped to the host's value, never
    /// raised: a smaller instruction request lowers that mod's hook interval to match; memory
    /// is at least 64 KiB when the host cap allows it, and never above the host cap. A host
    /// cap under 64 KiB is kept. `seal` raises memory further when an empty state needs more).
    #[must_use]
    pub fn for_mod(&self, instruction_budget: Option<u64>, memory_limit_bytes: Option<u64>) -> Self {
        let mut out = *self;
        if let Some(b) = instruction_budget {
            // A request below the host's cap is kept; the hook then runs at most that often,
            // so the first check cannot land past the mod's own budget.
            out.instruction_budget = b.max(1).min(self.instruction_budget);
            let interval = u32::try_from(out.instruction_budget).unwrap_or(u32::MAX);
            out.hook_interval = self.hook_interval.max(1).min(interval);
        }
        if let Some(m) = memory_limit_bytes {
            let floor = (64 * 1024).min(self.memory_limit_bytes);
            out.memory_limit_bytes = m.clamp(floor, self.memory_limit_bytes);
        }
        out
    }
}

/// The running call's budget, shared by the hook and the `pcall` wrappers.
#[derive(Debug)]
pub(crate) struct Budget {
    used: AtomicU64,
    limit: AtomicU64,
    tripped: AtomicBool,
    memory_tripped: AtomicBool,
    /// The origin of the two clock readings below.
    created: Instant,
    /// Milliseconds since `created` when the call began.
    began_millis: AtomicU64,
    /// Milliseconds this budget's earlier calls took, when several share it (a delivery pass).
    spent_millis: AtomicU64,
    /// [`LuaLimits::max_call_millis`].
    max_call_millis: AtomicU64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            used: AtomicU64::new(0),
            limit: AtomicU64::new(0),
            tripped: AtomicBool::new(false),
            memory_tripped: AtomicBool::new(false),
            created: Instant::now(),
            began_millis: AtomicU64::new(0),
            spent_millis: AtomicU64::new(0),
            max_call_millis: AtomicU64::new(0),
        }
    }
}

impl Budget {
    fn now_millis(&self) -> u64 {
        u64::try_from(self.created.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
    /// Starts a call with `limit` instructions.
    pub(crate) fn begin(&self, limit: u64) {
        self.used.store(0, Ordering::Relaxed);
        self.limit.store(limit, Ordering::Relaxed);
        self.tripped.store(false, Ordering::Relaxed);
        self.memory_tripped.store(false, Ordering::Relaxed);
        self.began_millis.store(self.now_millis(), Ordering::Relaxed);
        self.spent_millis.store(0, Ordering::Relaxed);
    }
    /// One call of a pass that shares this budget starts: the time between two of them belongs to
    /// the game and to other mods, and the watchdog must not count it.
    pub(crate) fn start_clock(&self) {
        self.began_millis.store(self.now_millis(), Ordering::Relaxed);
    }
    /// One call of such a pass ended.
    pub(crate) fn stop_clock(&self) {
        let ran = self.ran_millis();
        self.spent_millis.fetch_add(ran, Ordering::Relaxed);
    }
    fn ran_millis(&self) -> u64 {
        self.now_millis()
            .saturating_sub(self.began_millis.load(Ordering::Relaxed))
    }
    /// Whether the call has run past the watchdog.
    fn past_watchdog(&self) -> bool {
        let max = self.max_call_millis.load(Ordering::Relaxed);
        max > 0
            && self
                .spent_millis
                .load(Ordering::Relaxed)
                .saturating_add(self.ran_millis())
                > max
    }
    /// Instructions the call has used: hook intervals plus the host work charged to it.
    pub(crate) fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }
    /// Instructions the call may still use.
    pub(crate) fn left(&self) -> u64 {
        self.limit.load(Ordering::Relaxed).saturating_sub(self.used())
    }
    /// Marks the budget spent; the error the hook raises.
    pub(crate) fn exhausted(&self) -> mlua::Error {
        self.tripped.store(true, Ordering::Relaxed);
        mlua::Error::runtime(BUDGET_MESSAGE)
    }
    /// Marks a memory trip; the error the allocator raises.
    pub(crate) fn out_of_memory(&self) -> mlua::Error {
        self.memory_tripped.store(true, Ordering::Relaxed);
        mlua::Error::runtime(OOM_MESSAGE)
    }
    /// Whether the instruction budget ran out.
    pub(crate) fn tripped(&self) -> bool {
        self.tripped.load(Ordering::Relaxed)
    }
    /// Whether a memory error was seen (even one the script caught).
    pub(crate) fn memory_tripped(&self) -> bool {
        self.memory_tripped.load(Ordering::Relaxed)
    }
    /// Counts host work done for the script as `instructions`, and raises the budget error past
    /// the limit (the count hook never runs while a host function does).
    pub(crate) fn charge(&self, instructions: u64) -> mlua::Result<()> {
        let used = self.used.fetch_add(instructions, Ordering::Relaxed) + instructions;
        if used > self.limit.load(Ordering::Relaxed) || self.tripped() {
            return Err(self.exhausted());
        }
        Ok(())
    }
}

/// The message of the error the hook raises.
pub(crate) const BUDGET_MESSAGE: &str = "instruction budget exhausted";
/// Lua 5.1's out-of-memory message.
const OOM_MESSAGE: &str = "not enough memory";

/// Runs after the standard libraries load. Captures the raw functions as upvalues (unreachable
/// without `debug`) and replaces the globals that could swallow a budget error or reach outside.
const PRELUDE: &str = r#"
local tripped, log_line, mem_kib = ...
local raw_pcall, raw_xpcall, raw_resume, raw_tostring = pcall, xpcall, coroutine.resume, tostring
local raw_rep, raw_sort = string.rep, table.sort
local select, error, concat, unpack, tonumber, type = select, error, table.concat, unpack, tonumber, type
-- Lua 5.1's pcall and xpcall push their status without checking the stack, so a protected
-- function returning many values overruns the C frame. They run a closure that packs the
-- results into one table instead; `unpack` checks the stack.
local function pack(...) return { n = select('#', ...), ... } end
local function call_packed(f, ...) return pack(f(...)) end
local function pass_packed(ok, packed)
  if ok then return true, unpack(packed, 1, packed.n) end
  if tripped(packed) then error(packed, 0) end
  return false, packed
end
local function pass(ok, ...)
  if not ok and tripped((...)) then error((...), 0) end
  return ok, ...
end
pcall = function(f, ...) return pass_packed(raw_pcall(call_packed, f, ...)) end
-- The handler sees a trip first: it may not replace a memory or budget error with its own value.
local function guard(h)
  if type(h) ~= "function" then return h end
  return function(e)
    if tripped(e) then return e end
    return h(e)
  end
end
xpcall = function(f, h) return pass_packed(raw_xpcall(function() return pack(f()) end, guard(h))) end
coroutine.resume = function(co, ...) return pass(raw_resume(co, ...)) end
print = function(...)
  local parts = {}
  for i = 1, select('#', ...) do parts[i] = raw_tostring((select(i, ...))) end
  log_line(concat(parts, "\t"))
end
collectgarbage = function(opt)
  if opt == nil or opt == "count" then return mem_kib() end
  error("collectgarbage: mods may only ask for \"count\"", 2)
end
-- C code the hook never sees and the memory cap does not bound. Lua 5.1's `rep` of an empty string
-- loops `n` times over nothing (`n` may be a numeric string: `rep` converts it). Its sort has a
-- fixed pivot rule, so an array can be built that takes it n^2 / 4 comparisons in C: with a Lua
-- comparator each one is instructions the hook counts.
string.rep = function(s, n)
  if s == "" and tonumber(n) then return "" end
  return raw_rep(s, n)
end
local function less_than(a, b) return a < b end
table.sort = function(t, less)
  if less == nil then less = less_than end
  return raw_sort(t, less)
end
"#;

/// Globals removed from the base library (ADR-1751 § 3).
const REMOVED_GLOBALS: [&str; 9] = [
    "dofile",
    "loadfile",
    "load",
    "loadstring",
    "require",
    "module",
    "gcinfo",
    "newproxy",
    "package",
];

/// A fresh sandboxed state, its budget cell and the limits it runs under. The memory cap is
/// applied by [`seal`] once the host API is installed.
pub(crate) struct Sandbox {
    pub(crate) lua: Lua,
    pub(crate) budget: Arc<Budget>,
}

/// Builds the state: whitelisted libraries, prelude, instruction hook. `log` receives `print`
/// lines.
pub(crate) fn new_state(limits: &LuaLimits) -> Result<Sandbox, ModError> {
    let internal =
        |e: mlua::Error| ModError::new(LuaErrorCode::Runtime, format!("creating the Lua state: {e}"));
    let lua = Lua::new_with(
        // Lua 5.1 opens `coroutine` with the base library.
        StdLib::STRING | StdLib::TABLE | StdLib::MATH,
        LuaOptions::default(),
    )
    .map_err(internal)?;
    let budget = Arc::new(Budget::default());
    budget
        .max_call_millis
        .store(u64::from(limits.max_call_millis), Ordering::Relaxed);

    let globals = lua.globals();
    for name in REMOVED_GLOBALS {
        globals.raw_set(name, Value::Nil).map_err(internal)?;
    }
    for lib in ["io", "os", "debug"] {
        globals.raw_set(lib, Value::Nil).map_err(internal)?;
    }
    let string: mlua::Table = globals.raw_get("string").map_err(internal)?;
    string.raw_set("dump", Value::Nil).map_err(internal)?;
    crate::pattern::install(&lua, &string, &budget, limits.memory_limit_bytes).map_err(internal)?;

    let trip_budget = Arc::clone(&budget);
    let tripped = lua
        .create_function(move |_lua, err: Value| {
            // The allocator's message is exactly this; a script's own text that only begins with
            // it is an ordinary error. `error(OOM_MESSAGE, 0)` still reads as a trip, and only
            // ends the call of the mod that raised it.
            if let Value::String(s) = &err
                && s.as_bytes() == OOM_MESSAGE.as_bytes()
            {
                trip_budget.memory_tripped.store(true, Ordering::Relaxed);
            }
            // A host function whose allocation failed raises mlua's memory error. Only that
            // evidence trips: a heap that is merely near its cap does not turn every caught error
            // into a memory trip (audit wave 5, luasave H2). An `xpcall` handler sees this check
            // before it can replace the value (`guard` in the prelude).
            if let Value::Error(error) = &err
                && is_memory_error(error)
            {
                trip_budget.memory_tripped.store(true, Ordering::Relaxed);
            }
            Ok(trip_budget.tripped() || trip_budget.memory_tripped())
        })
        .map_err(internal)?;
    let log_line = lua
        .create_function(|lua, line: mlua::LuaString| {
            crate::host::push_log(lua, &String::from_utf8_lossy(&line.as_bytes()));
            Ok(())
        })
        .map_err(internal)?;
    let mem_kib = lua
        .create_function(|lua, ()| Ok(lua.used_memory() as f64 / 1024.0))
        .map_err(internal)?;
    lua.load(PRELUDE)
        .set_name("=regia.prelude")
        .set_mode(mlua::chunk::ChunkMode::Text)
        .call::<()>((tripped, log_line, mem_kib))
        .map_err(internal)?;

    let hook_budget = Arc::clone(&budget);
    let interval = limits.hook_interval.max(1);
    lua.set_global_hook(
        HookTriggers::new().every_nth_instruction(interval),
        move |_lua, _debug| {
            let used =
                hook_budget.used.fetch_add(u64::from(interval), Ordering::Relaxed) + u64::from(interval);
            if used > hook_budget.limit.load(Ordering::Relaxed)
                || hook_budget.tripped()
                || hook_budget.past_watchdog()
            {
                return Err(hook_budget.exhausted());
            }
            Ok(VmState::Continue)
        },
    )
    .map_err(internal)?;
    Ok(Sandbox { lua, budget })
}

/// Applies the memory cap (after the host API is installed, so setup never fails on a small
/// cap).
pub(crate) fn seal(lua: &Lua, limits: &LuaLimits) -> Result<(), ModError> {
    let cap = usize::try_from(limits.memory_limit_bytes).unwrap_or(usize::MAX);
    let floor = lua.used_memory() + 16 * 1024;
    if cap < floor {
        tracing::warn!(target: "regia_script_lua", cap, floor, "the memory cap is below what an empty state needs: raised");
    }
    lua.set_memory_limit(cap.max(floor))
        .map(|_| ())
        .map_err(|e| ModError::new(LuaErrorCode::Runtime, format!("setting the memory cap: {e}")))
}

/// Whether `error`, or an error it wraps (a callback's cause, a context), is an allocation
/// failure.
fn is_memory_error(error: &mlua::Error) -> bool {
    match error {
        mlua::Error::MemoryError(_) => true,
        mlua::Error::CallbackError { cause, .. } | mlua::Error::WithContext { cause, .. } => {
            is_memory_error(cause)
        }
        _ => false,
    }
}

/// Compiles and runs a text chunk named `name`.
pub(crate) fn load_text(lua: &Lua, source: &str, name: &str) -> mlua::Result<mlua::Function> {
    lua.load(source)
        .set_name(format!("={name}"))
        .set_mode(mlua::chunk::ChunkMode::Text)
        .into_function()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_watchdog_adds_up_the_calls_of_a_pass_until_the_next_one_begins() {
        let budget = Budget::default();
        budget.max_call_millis.store(3_600_000, Ordering::Relaxed);
        budget.begin(100);
        budget.stop_clock();
        budget.start_clock();
        assert!(!budget.past_watchdog());
        budget.spent_millis.store(3_600_001, Ordering::Relaxed);
        budget.start_clock();
        assert!(
            budget.past_watchdog(),
            "starting a call keeps what the pass spent"
        );
        budget.begin(100);
        assert!(!budget.past_watchdog(), "a new pass starts from nothing");
    }

    #[test]
    fn requested_budgets_are_clamped_to_the_host() {
        let host = LuaLimits::default();
        let asked_more = host.for_mod(Some(u64::MAX), Some(u64::MAX));
        assert_eq!(asked_more.instruction_budget, host.instruction_budget);
        assert_eq!(asked_more.memory_limit_bytes, host.memory_limit_bytes);
        let asked_less = host.for_mod(Some(10), Some(10));
        assert_eq!(asked_less.instruction_budget, 10);
        assert_eq!(asked_less.hook_interval, 10);
        let modest = host.for_mod(Some(100), None);
        assert_eq!((modest.instruction_budget, modest.hook_interval), (100, 100));
        let zero = host.for_mod(Some(0), None);
        assert_eq!((zero.instruction_budget, zero.hook_interval), (1, 1));
        let large = host.for_mod(Some(5_000), None);
        assert_eq!(
            (large.instruction_budget, large.hook_interval),
            (5_000, host.hook_interval)
        );
        assert_eq!(asked_less.memory_limit_bytes, 64 * 1024);
        assert_eq!(host.for_mod(None, None), host);
        let tight = LuaLimits {
            memory_limit_bytes: 32 * 1024,
            ..LuaLimits::default()
        };
        assert_eq!(tight.for_mod(None, Some(32 * 1024)).memory_limit_bytes, 32 * 1024);
        assert_eq!(
            tight.for_mod(None, Some(128 * 1024)).memory_limit_bytes,
            32 * 1024
        );
        assert_eq!(tight.for_mod(None, None).memory_limit_bytes, 32 * 1024);
    }
}
