// SPDX-License-Identifier: MPL-2.0
//! Host and presentation runtimes, the bounded host command queue and the snapshot hook
//! (spec 84 § 15, ADR-8466).
//!
//! A networked game runs its mods on two sides. The *host* (the authority: a dedicated server, a
//! listen server's host, or a single-player game) may ask the game to do something through
//! `regia.command(name, value)`; the game drains those requests with
//! [`LuaMods::drain_host_commands`](crate::LuaMods::drain_host_commands) and applies each through
//! its own command path, exactly as it would a player's. A *presentation* runtime (a client) has no
//! `regia.command` at all: it reads, draws and emits events for its own UI, but nothing it runs can
//! reach the authority's queue.
//!
//! The game publishes what scripts may read with
//! [`LuaMods::publish_snapshot`](crate::LuaMods::publish_snapshot). An unchanged snapshot costs
//! nothing: no hook runs and no table is rebuilt.
//!
//! Requirements from DO27's script bridge (`ir-script/src/lib.rs` at d6ed37677, regia-absorb-1,
//! requirements only; nothing copied): the host-only enqueue, the bounded queue that refuses with a
//! reason instead of growing, finite payloads, and the skipped rebuild of an unchanged published
//! state. The command names are the game's (an allow-list), not the engine's.

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use mlua::{Lua, Value};
use serde::{Deserialize, Serialize};

use crate::value::ScriptValue;

/// Commands one mod may have waiting for the game to drain, by default
/// ([`HostCommandRules::max_pending_per_mod`]).
pub const DEFAULT_MAX_PENDING_COMMANDS: usize = 32;
/// Bytes one command's value may take, by default ([`HostCommandRules::max_command_bytes`]).
pub const DEFAULT_MAX_COMMAND_BYTES: u64 = 4096;
/// Most bytes a published snapshot may take
/// ([`LuaMods::publish_snapshot`](crate::LuaMods::publish_snapshot) refuses a larger one with
/// `lua_too_large`).
pub const MAX_SNAPSHOT_BYTES: u64 = 16 * 1024 * 1024;
/// Longest command name, in bytes.
pub const MAX_COMMAND_NAME_BYTES: usize = 64;

/// Instructions one `regia.command` costs, plus one per this many bytes of the value it converts.
pub(crate) const COMMAND_BASE_COST: u64 = 16;
pub(crate) const COMMAND_BYTES_PER_INSTRUCTION: u64 = 64;
/// Instructions building a snapshot table costs, plus one per this many bytes of it.
pub(crate) const SNAPSHOT_BASE_COST: u64 = 64;
pub(crate) const SNAPSHOT_BYTES_PER_INSTRUCTION: u64 = 256;
/// Registry key of the mod's cached snapshot table.
pub(crate) const SNAPSHOT_KEY: &str = "regia.snapshot";

/// Which side of a networked game a mod host runs on (ADR-8466).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptAuthority {
    /// The authority: `regia.command` exists and queues requests for the game. The default: a
    /// single-player game is its own authority.
    #[default]
    Host,
    /// A client or presentation shell: no `regia.command`. Hooks, events, `regia.snapshot` and
    /// the local world only.
    Presentation,
}

impl ScriptAuthority {
    /// The value of `regia.authority` and of the agent's `authority` field: `"host"` or
    /// `"presentation"`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ScriptAuthority::Host => "host",
            ScriptAuthority::Presentation => "presentation",
        }
    }
}

/// What a host runtime's `regia.command` accepts. The game registers its command names; a name it
/// did not register is refused (`unknown_command`), so a mod cannot invent a command the game
/// never wrote a handler for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCommandRules {
    /// The command names the game handles. Empty (the default): every command is refused.
    pub names: BTreeSet<String>,
    /// Commands one mod may have waiting for [`LuaMods::drain_host_commands`](crate::LuaMods::drain_host_commands);
    /// past it `regia.command` refuses with `queue_full`.
    pub max_pending_per_mod: usize,
    /// Bytes one command's value may take (about its size on the heap); past it, `too_large`.
    pub max_command_bytes: u64,
}

impl Default for HostCommandRules {
    fn default() -> Self {
        Self {
            names: BTreeSet::new(),
            max_pending_per_mod: DEFAULT_MAX_PENDING_COMMANDS,
            max_command_bytes: DEFAULT_MAX_COMMAND_BYTES,
        }
    }
}

impl HostCommandRules {
    /// The default caps with `names` registered.
    ///
    /// ```
    /// use regia_script_lua::{DEFAULT_MAX_PENDING_COMMANDS, HostCommandRules};
    ///
    /// let rules = HostCommandRules::new(["build", "order"]);
    /// assert!(rules.names.contains("build"));
    /// assert_eq!(rules.max_pending_per_mod, DEFAULT_MAX_PENDING_COMMANDS);
    /// ```
    #[must_use]
    pub fn new<S: Into<String>>(names: impl IntoIterator<Item = S>) -> Self {
        Self {
            names: names.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }
}

/// A request a host-side mod queued with `regia.command(name, value)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostCommand {
    /// The mod that queued it.
    pub mod_id: String,
    /// The command name (one of [`HostCommandRules::names`]).
    pub name: String,
    /// Its value (`nil` when the script passed none). Numbers in it are finite.
    pub value: ScriptValue,
}

/// Why `regia.command` refused a command. The script receives `false` and [`Self::reason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandRefusal {
    /// The name is not registered.
    UnknownCommand,
    /// The value is not plain data (a function, a coroutine, ...) or holds a non-finite number.
    BadValue,
    /// The value is over [`HostCommandRules::max_command_bytes`].
    TooLarge,
    /// The mod already has [`HostCommandRules::max_pending_per_mod`] commands waiting.
    QueueFull,
}

impl CommandRefusal {
    /// The reason string the script receives.
    pub(crate) fn reason(self) -> &'static str {
        match self {
            CommandRefusal::UnknownCommand => "unknown_command",
            CommandRefusal::BadValue => "bad_value",
            CommandRefusal::TooLarge => "too_large",
            CommandRefusal::QueueFull => "queue_full",
        }
    }
}

/// One mod's share of the authority state, kept in its `ModRuntime`.
#[derive(Debug, Default)]
pub(crate) struct ModAuthority {
    pub(crate) authority: ScriptAuthority,
    pub(crate) rules: Arc<HostCommandRules>,
    /// Commands waiting for the drain, in call order.
    pub(crate) pending: VecDeque<HostCommand>,
    /// Commands `regia.command` refused.
    pub(crate) refused: u64,
    /// The latest published snapshot and its generation (0: none yet).
    pub(crate) snapshot: Option<Arc<ScriptValue>>,
    pub(crate) generation: u64,
    /// The snapshot's weight (`ScriptValue::weight`), measured once when it was published.
    pub(crate) weight: u64,
    /// The generation the cached table in the registry was built from (0: none).
    pub(crate) built: u64,
    /// Snapshot tables built so far.
    pub(crate) builds: u64,
}

impl ModAuthority {
    pub(crate) fn new(authority: ScriptAuthority, rules: Arc<HostCommandRules>) -> Self {
        Self {
            authority,
            rules,
            ..Self::default()
        }
    }

    /// Checks one command against the rules and queues it. Also returns the instructions the
    /// check costs: [`COMMAND_BASE_COST`] plus the value's conversion (a script that offers one big
    /// table over and over pays for every walk, refused or not).
    pub(crate) fn offer(
        &mut self,
        mod_id: &str,
        name: &[u8],
        value: &Value,
    ) -> (Result<(), CommandRefusal>, u64) {
        let mut cost = COMMAND_BASE_COST;
        let checked = self.check(name, value, &mut cost);
        let outcome = match checked {
            Ok((name, value)) => {
                self.pending.push_back(HostCommand {
                    mod_id: mod_id.to_owned(),
                    name,
                    value,
                });
                Ok(())
            }
            Err(refusal) => {
                self.refused += 1;
                Err(refusal)
            }
        };
        (outcome, cost)
    }

    fn check(
        &self,
        name: &[u8],
        value: &Value,
        cost: &mut u64,
    ) -> Result<(String, ScriptValue), CommandRefusal> {
        // The cheap checks first: an unregistered name or a full queue is refused before the value
        // is even read.
        let name = std::str::from_utf8(name)
            .ok()
            .filter(|n| !n.is_empty() && n.len() <= MAX_COMMAND_NAME_BYTES)
            .filter(|n| self.rules.names.contains(*n))
            .ok_or(CommandRefusal::UnknownCommand)?;
        if self.pending.len() >= self.rules.max_pending_per_mod {
            return Err(CommandRefusal::QueueFull);
        }
        let value = ScriptValue::from_lua(value).map_err(|_| CommandRefusal::BadValue)?;
        let weight = value.weight();
        *cost = cost.saturating_add(weight / COMMAND_BYTES_PER_INSTRUCTION);
        if !all_finite(&value) {
            return Err(CommandRefusal::BadValue);
        }
        if weight > self.rules.max_command_bytes {
            return Err(CommandRefusal::TooLarge);
        }
        Ok((name.to_owned(), value))
    }
}

/// Whether every number in `value` (vectors too) is finite.
pub(crate) fn all_finite(value: &ScriptValue) -> bool {
    match value {
        ScriptValue::Number(n) => n.is_finite(),
        ScriptValue::Vec3(v) => v.iter().all(|c| c.is_finite()),
        ScriptValue::List(items) => items.iter().all(all_finite),
        ScriptValue::Map(map) => map.values().all(all_finite),
        ScriptValue::Nil | ScriptValue::Bool(_) | ScriptValue::String(_) | ScriptValue::Entity(_) => true,
    }
}

/// The value of `regia.snapshot()`: the mod's table for the latest snapshot, built once per
/// published generation and charged to the running call's budget; `nil` before the first one.
pub(crate) fn snapshot_table(lua: &Lua) -> mlua::Result<Value> {
    let (snapshot, generation, weight, budget) = {
        let Some(rt) = lua.app_data_ref::<crate::host::ModRuntime>() else {
            return Ok(Value::Nil);
        };
        let Some(snapshot) = rt.authority.snapshot.clone() else {
            return Ok(Value::Nil);
        };
        if rt.authority.built == rt.authority.generation {
            drop(rt);
            return lua.named_registry_value::<Value>(SNAPSHOT_KEY);
        }
        (
            snapshot,
            rt.authority.generation,
            rt.authority.weight,
            rt.budget(),
        )
    };
    budget.charge(SNAPSHOT_BASE_COST.saturating_add(weight / SNAPSHOT_BYTES_PER_INSTRUCTION))?;
    let table = snapshot.to_lua(lua)?;
    lua.set_named_registry_value(SNAPSHOT_KEY, &table)?;
    if let Some(mut rt) = lua.app_data_mut::<crate::host::ModRuntime>() {
        rt.authority.built = generation;
        rt.authority.builds += 1;
    }
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_finite_numbers_anywhere_are_found() {
        assert!(all_finite(&ScriptValue::Number(1.0)));
        assert!(!all_finite(&ScriptValue::Number(f64::NAN)));
        assert!(!all_finite(&ScriptValue::Vec3([0.0, f32::INFINITY, 0.0])));
        assert!(!all_finite(&ScriptValue::List(vec![
            ScriptValue::Nil,
            ScriptValue::Number(f64::NEG_INFINITY)
        ])));
        let mut map = std::collections::BTreeMap::new();
        map.insert(
            "a".to_owned(),
            ScriptValue::List(vec![ScriptValue::Number(f64::NAN)]),
        );
        assert!(!all_finite(&ScriptValue::Map(map)));
        assert!(all_finite(&ScriptValue::String("nan".into())));
    }

    #[test]
    fn refusal_reasons_are_lowercase_codes() {
        for refusal in [
            CommandRefusal::UnknownCommand,
            CommandRefusal::BadValue,
            CommandRefusal::TooLarge,
            CommandRefusal::QueueFull,
        ] {
            let reason = refusal.reason();
            assert!(
                reason.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{reason}"
            );
        }
        assert_eq!(ScriptAuthority::default(), ScriptAuthority::Host);
        assert_eq!(ScriptAuthority::Presentation.name(), "presentation");
    }
}
