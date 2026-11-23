// SPDX-License-Identifier: MPL-2.0
//! Mod state that outlives a Lua state (`docs/specs/84-lua-mods.md` § 11, ADR-1943): the plain-data
//! tree behind `regia.state`, and the `luastate` format that carries it in a save game.
//!
//! A [`StateTree`] is a flat list of tables, so shared tables and cycles survive and nothing about
//! it recurses when decoded. Capturing reads raw table fields only: no metamethod and no Lua code
//! runs, and at most a few Lua references are held per nesting level (the binding's reference stack
//! is small on Lua 5.1, so a state with many tables must not hold one reference per table).
//! Building one back makes its tables in a holder table in the Lua heap, for the same reason.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use mlua::{Lua, Table, Value};
use regia_core::math::Rng;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{LuaErrorCode, ModError};
use crate::sandbox::LuaLimits;
use crate::value::{LuaVec3, lua_type};

/// The current `luastate` version. Version 1 is the first: no migration step exists yet. A v2 adds
/// a v1 → v2 step in [`decode`] and lists it in [`luastate_migration_sources`].
pub const LUASTATE_FORMAT_VERSION: u32 = 1;
/// The first four bytes of a `luastate` blob.
pub const LUASTATE_MAGIC: &[u8; 4] = b"RLST";
/// The key of the save section holding the blob.
pub const LUA_STATE_SECTION: &str = "regia_script_lua::LuaState";
/// Deepest nesting of tables in a state. Deeper needs a flatter layout; a cycle is not nested.
pub const MAX_STATE_DEPTH: usize = 128;
/// Longest name of a saved entity, in bytes (the limit `regia.spawn` applies).
const MAX_ENTITY_NAME: usize = 256;
/// Longest table path an error names.
const MAX_PATH: usize = 160;
/// Bytes a blob takes beyond its entries: the header and the entry count.
const CONTAINER_BYTES: u64 = 16;

/// A table key in a saved state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StateKey {
    /// A number key.
    Number(f64),
    /// A string key (Lua strings are bytes).
    String(Vec<u8>),
    /// A boolean key.
    Bool(bool),
}

/// A value in a saved state. Plain data only: a `vec3` is three numbers, not a host object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StateValue {
    /// A boolean.
    Bool(bool),
    /// A number.
    Number(f64),
    /// A string (Lua strings are bytes).
    String(Vec<u8>),
    /// A `regia.vec3`.
    Vec3([f32; 3]),
    /// The table at this index of [`StateTree::tables`].
    Table(u32),
}

/// The plain-data tree of one mod's `regia.state`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateTree {
    /// Every table, each with its entries sorted by key; table 0 is `regia.state` itself. The
    /// same value captures to the same list whatever order the keys were inserted in.
    pub tables: Vec<Vec<(StateKey, StateValue)>>,
}

impl Default for StateTree {
    fn default() -> Self {
        Self {
            tables: vec![Vec::new()],
        }
    }
}

/// An entity a mod owned when it was saved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedModEntity {
    /// Its name.
    pub name: String,
    /// Its local translation.
    pub position: [f32; 3],
}

impl SavedModEntity {
    /// Whether a load takes this entity back: its name fits and its position is finite. A save
    /// keeps only such entities (audit wave 5, luasave: a save the load refuses would disable the
    /// mod and strand its state).
    #[must_use]
    pub fn restorable(&self) -> bool {
        self.name.len() <= MAX_ENTITY_NAME && self.position.iter().all(|component| component.is_finite())
    }
}

/// One mod's saved state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModState {
    /// The mod id.
    pub id: String,
    /// The mod's own version string when it was saved.
    pub mod_version: String,
    /// The `regia.state_version` of that state.
    pub state_version: u32,
    /// Its random number generator.
    pub rng: Rng,
    /// `regia.state`.
    pub tree: StateTree,
    /// The entities it owned.
    pub entities: Vec<SavedModEntity>,
}

/// The `luastate` payload.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LuaStateFile {
    /// Every saved mod, in id order.
    pub mods: Vec<ModState>,
}

/// What became of a mod's state across a load or a reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StateCarry {
    /// Nothing was saved for it: the mod started fresh.
    Fresh,
    /// The state came across unchanged.
    Carried,
    /// `on_migrate` turned the state from `from` to `to`.
    Migrated {
        /// The version of the old state.
        from: u32,
        /// The version of the new code.
        to: u32,
    },
    /// The versions differ and the mod has no `on_migrate`, or the old state could not be read:
    /// `regia.state` is what the new code set up.
    Reset {
        /// The version of the old state.
        from: u32,
        /// The version of the new code.
        to: u32,
    },
}

impl StateCarry {
    /// The name `on_load` and the agent methods use.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            StateCarry::Fresh => "fresh",
            StateCarry::Carried => "carried",
            StateCarry::Migrated { .. } => "migrated",
            StateCarry::Reset { .. } => "reset",
        }
    }
}

fn state_error(message: impl Into<String>) -> ModError {
    ModError::new(LuaErrorCode::State, message)
}

fn bad_value(path: &str, message: impl std::fmt::Display) -> ModError {
    ModError::new(LuaErrorCode::BadValue, format!("{path}: {message}"))
}

fn too_large(message: impl Into<String>) -> ModError {
    ModError::new(LuaErrorCode::TooLarge, message)
}

/// The walk's running totals.
struct Capture<'a> {
    limits: &'a LuaLimits,
    ids: HashMap<usize, u32>,
    tables: Vec<Vec<(StateKey, StateValue)>>,
    items: usize,
    bytes: u64,
}

impl Capture<'_> {
    fn count(&mut self) -> Result<(), ModError> {
        self.items += 1;
        if self.items > self.limits.max_state_items {
            return Err(too_large(format!(
                "regia.state holds more than {} items",
                self.limits.max_state_items
            )));
        }
        Ok(())
    }

    fn string(&mut self, s: &mlua::LuaString) -> Result<Vec<u8>, ModError> {
        let bytes = s.as_bytes().to_vec();
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        if self.bytes > self.limits.max_state_bytes {
            return Err(too_large(format!(
                "regia.state holds more than {} bytes of strings",
                self.limits.max_state_bytes
            )));
        }
        Ok(bytes)
    }

    fn key(&mut self, key: &Value, path: &str) -> Result<StateKey, ModError> {
        Ok(match key {
            Value::Integer(i) => StateKey::Number(*i as f64),
            Value::Number(n) => StateKey::Number(*n),
            Value::Boolean(b) => StateKey::Bool(*b),
            Value::String(s) => StateKey::String(self.string(s)?),
            other => {
                return Err(bad_value(
                    path,
                    format!("a {} cannot be a key in saved state", lua_type(other)),
                ));
            }
        })
    }

    fn value(
        &mut self,
        value: &Value,
        path: &str,
        key: &StateKey,
        depth: usize,
    ) -> Result<StateValue, ModError> {
        Ok(match value {
            Value::Boolean(b) => StateValue::Bool(*b),
            Value::Integer(i) => StateValue::Number(*i as f64),
            Value::Number(n) => StateValue::Number(*n),
            Value::String(s) => StateValue::String(self.string(s)?),
            Value::Table(table) => match self.ids.get(&(table.to_pointer() as usize)) {
                Some(id) => StateValue::Table(*id),
                None => StateValue::Table(self.table(table, &child_path(path, key), depth + 1)?),
            },
            Value::UserData(ud) => match ud.borrow::<LuaVec3>() {
                Ok(v) => StateValue::Vec3(v.0.to_array()),
                Err(_) => {
                    return Err(bad_value(
                        &child_path(path, key),
                        "userdata cannot be saved (an entity handle does not outlive a load: \
                         save its name and regia.find it in on_load)",
                    ));
                }
            },
            other => {
                return Err(bad_value(
                    &child_path(path, key),
                    format!("a {} cannot be saved", lua_type(other)),
                ));
            }
        })
    }

    fn table(&mut self, table: &Table, path: &str, depth: usize) -> Result<u32, ModError> {
        if depth >= MAX_STATE_DEPTH {
            return Err(bad_value(
                path,
                format!("tables nest at most {MAX_STATE_DEPTH} levels in saved state"),
            ));
        }
        if table.metatable().is_some() {
            return Err(bad_value(path, "a table with a metatable cannot be saved"));
        }
        self.count()?;
        let id =
            u32::try_from(self.tables.len()).map_err(|_| too_large("regia.state has too many tables"))?;
        self.ids.insert(table.to_pointer() as usize, id);
        self.tables.push(Vec::new());
        let mut entries = Vec::new();
        for pair in table.pairs::<Value, Value>() {
            let (key, value) = pair.map_err(|e| bad_value(path, e))?;
            self.count()?;
            let key = self.key(&key, path)?;
            let value = self.value(&value, path, &key, depth)?;
            entries.push((key, value));
        }
        entries.sort_by(|a, b| key_order(&a.0, &b.0));
        self.tables[id as usize] = entries;
        Ok(id)
    }
}

/// The path an error names for `key` below `path`, at most about [`MAX_PATH`] bytes. A key is cut
/// before it is copied: every child table of a key copies the path, so a path that grew with a
/// long key cost (children x key length) bytes on each save, unseen by the budget (ADR-5870).
fn child_path(path: &str, key: &StateKey) -> String {
    if path.len() >= MAX_PATH {
        return path.to_owned();
    }
    let mut out = match key {
        StateKey::Number(n) => format!("{path}[{n}]"),
        StateKey::Bool(b) => format!("{path}[{b}]"),
        StateKey::String(s) => {
            let keep = s.len().min(MAX_PATH - path.len());
            format!("{path}.{}", String::from_utf8_lossy(&s[..keep]))
        }
    };
    if out.len() > MAX_PATH {
        let mut cut = MAX_PATH;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out
}

/// Numbers, then strings, then booleans.
fn key_order(a: &StateKey, b: &StateKey) -> Ordering {
    match (a, b) {
        (StateKey::Number(x), StateKey::Number(y)) => x.total_cmp(y),
        (StateKey::String(x), StateKey::String(y)) => x.cmp(y),
        (StateKey::Bool(x), StateKey::Bool(y)) => x.cmp(y),
        (StateKey::Number(_), _) | (StateKey::String(_), StateKey::Bool(_)) => Ordering::Less,
        _ => Ordering::Greater,
    }
}

/// Renumbers `tables` breadth first over their sorted entries, so the ids depend on the data and
/// not on the order `pairs` happened to walk it.
fn canonical(tables: Vec<Vec<(StateKey, StateValue)>>) -> Vec<Vec<(StateKey, StateValue)>> {
    let mut new_id = vec![u32::MAX; tables.len()];
    let mut order: Vec<usize> = vec![0];
    new_id[0] = 0;
    let mut next = 0;
    while next < order.len() {
        for (_, value) in &tables[order[next]] {
            if let StateValue::Table(id) = value
                && new_id[*id as usize] == u32::MAX
            {
                new_id[*id as usize] = order.len() as u32;
                order.push(*id as usize);
            }
        }
        next += 1;
    }
    order
        .iter()
        .map(|&old| {
            tables[old]
                .iter()
                .map(|(key, value)| {
                    let value = match value {
                        StateValue::Table(id) => StateValue::Table(new_id[*id as usize]),
                        other => other.clone(),
                    };
                    (key.clone(), value)
                })
                .collect()
        })
        .collect()
}

impl StateTree {
    /// Reads `root` and everything it reaches. Refuses functions, coroutines, userdata other than
    /// `vec3`, tables with a metatable, and anything past the caps in `limits`. Runs no Lua code.
    pub(crate) fn capture(root: &Table, limits: &LuaLimits) -> Result<Self, ModError> {
        let mut walk = Capture {
            limits,
            ids: HashMap::new(),
            tables: Vec::new(),
            items: 0,
            bytes: 0,
        };
        walk.table(root, "regia.state", 0)?;
        Ok(Self {
            tables: canonical(walk.tables),
        })
    }

    /// Builds the table in `lua`. The caller has checked the tree ([`check`](Self::check)).
    pub(crate) fn build(&self, lua: &Lua) -> mlua::Result<Table> {
        self.build_into(lua, lua.create_table()?)
    }

    /// Builds the table in `lua` with `root` as table 0, emptied first, so whoever holds `root`
    /// sees the restored state and a reference to table 0 inside the tree reaches `root`.
    pub(crate) fn build_into(&self, lua: &Lua, root: Table) -> mlua::Result<Table> {
        root.clear()?;
        let holder = lua.create_table_with_capacity(self.tables.len(), 0)?;
        holder.raw_set(1, root)?;
        for index in 1..self.tables.len() {
            holder.raw_set(index + 1, lua.create_table()?)?;
        }
        for (index, entries) in self.tables.iter().enumerate() {
            let table: Table = holder.raw_get(index + 1)?;
            for (key, value) in entries {
                let value = match value {
                    StateValue::Bool(b) => Value::Boolean(*b),
                    StateValue::Number(n) => Value::Number(*n),
                    StateValue::String(s) => Value::String(lua.create_string(s)?),
                    StateValue::Vec3(v) => {
                        crate::value::counted_vec3(lua, regia_core::math::Vec3::from_array(*v))?
                    }
                    StateValue::Table(id) => holder.raw_get::<Value>(*id as usize + 1)?,
                };
                match key {
                    StateKey::Number(n) => table.raw_set(*n, value)?,
                    StateKey::Bool(b) => table.raw_set(*b, value)?,
                    StateKey::String(s) => table.raw_set(lua.create_string(s)?, value)?,
                }
            }
        }
        holder.raw_get(1)
    }

    /// Tables plus entries, the count [`LuaLimits::max_state_items`] caps.
    #[must_use]
    pub fn items(&self) -> usize {
        self.tables.len() + self.tables.iter().map(Vec::len).sum::<usize>()
    }

    /// Checks a tree that came out of a save file: it has a root, every table reference is in
    /// range, no key is NaN and it is inside `limits`.
    pub fn check(&self, limits: &LuaLimits) -> Result<(), ModError> {
        if self.tables.is_empty() {
            return Err(state_error("the saved state has no root table"));
        }
        if self.items() > limits.max_state_items {
            return Err(too_large(format!(
                "the saved state has {} items, more than the {} allowed",
                self.items(),
                limits.max_state_items
            )));
        }
        if let Some(fault) = self.structural_fault() {
            return Err(state_error(fault));
        }
        self.check_depth()
    }

    /// A fault no version of a mod could load, or `None`: no root table, a NaN key, a reference
    /// to a table the tree does not hold, or one key twice in a table under Lua's key equality
    /// (`-0` and `+0` are one key; audit wave 6, luasave L1). Unlike a cap, such an entry is not
    /// kept for a later version of the mod (audit wave 6, luasave H2).
    fn structural_fault(&self) -> Option<&'static str> {
        /// A key as Lua compares it.
        #[derive(PartialEq, Eq, Hash)]
        enum LuaKey<'a> {
            Number(u64),
            String(&'a [u8]),
            Bool(bool),
        }
        if self.tables.is_empty() {
            return Some("the saved state has no root table");
        }
        let mut seen = HashSet::new();
        for entries in &self.tables {
            seen.clear();
            for (key, value) in entries {
                let lua_key = match key {
                    StateKey::Number(n) if n.is_nan() => return Some("the saved state has a NaN key"),
                    StateKey::Number(n) if *n == 0.0 => LuaKey::Number(0),
                    StateKey::Number(n) => LuaKey::Number(n.to_bits()),
                    StateKey::String(s) => LuaKey::String(s),
                    StateKey::Bool(b) => LuaKey::Bool(*b),
                };
                if !seen.insert(lua_key) {
                    return Some("the saved state has a key twice in one table");
                }
                if let StateValue::Table(id) = value
                    && *id as usize >= self.tables.len()
                {
                    return Some("the saved state refers to a table it does not hold");
                }
            }
        }
        None
    }

    /// Refuses a tree whose tables nest deeper than [`MAX_STATE_DEPTH`] (audit wave 5, luasave:
    /// such a state restores, and then its next save fails). A table's depth here is its shortest
    /// distance from the root, never more than the depth a capture found it at, so no tree a
    /// capture wrote is refused; a table reached again (shared or a cycle) is not nested deeper.
    fn check_depth(&self) -> Result<(), ModError> {
        let mut depth = vec![usize::MAX; self.tables.len()];
        let mut queue = std::collections::VecDeque::from([0usize]);
        depth[0] = 0;
        while let Some(id) = queue.pop_front() {
            let next = depth[id] + 1;
            for (_, value) in &self.tables[id] {
                if let StateValue::Table(child) = value {
                    let child = *child as usize;
                    if depth[child] != usize::MAX {
                        continue;
                    }
                    if next >= MAX_STATE_DEPTH {
                        return Err(state_error(format!(
                            "the saved state nests tables deeper than {MAX_STATE_DEPTH} levels"
                        )));
                    }
                    depth[child] = next;
                    queue.push_back(child);
                }
            }
        }
        Ok(())
    }

    /// The tree as JSON for the agent protocol. A sequence is an array and any other table an
    /// object with its keys as text; a `vec3` is `{ "vec3": [x, y, z] }`; a table already shown, or
    /// past 16 levels, is `{ "ref": table_index }`.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut seen = HashSet::new();
        self.table_json(0, 0, &mut seen)
    }

    fn table_json(&self, id: usize, depth: usize, seen: &mut HashSet<usize>) -> serde_json::Value {
        let Some(entries) = self.tables.get(id) else {
            return serde_json::Value::Null;
        };
        if depth >= 16 || !seen.insert(id) {
            return json!({ "ref": id });
        }
        let value = |v: &StateValue, seen: &mut HashSet<usize>| match v {
            StateValue::Bool(b) => json!(b),
            StateValue::Number(n) => json!(n),
            StateValue::String(s) => json!(String::from_utf8_lossy(s)),
            StateValue::Vec3(v) => json!({ "vec3": v }),
            StateValue::Table(child) => self.table_json(*child as usize, depth + 1, seen),
        };
        let is_list = !entries.is_empty()
            && entries
                .iter()
                .enumerate()
                .all(|(i, (key, _))| matches!(key, StateKey::Number(n) if *n == (i + 1) as f64));
        if is_list {
            return serde_json::Value::Array(entries.iter().map(|(_, v)| value(v, seen)).collect());
        }
        serde_json::Value::Object(
            entries
                .iter()
                .map(|(key, v)| {
                    let key = match key {
                        StateKey::Number(n) => n.to_string(),
                        StateKey::Bool(b) => b.to_string(),
                        StateKey::String(s) => String::from_utf8_lossy(s).into_owned(),
                    };
                    (key, value(v, seen))
                })
                .collect(),
        )
    }
}

impl ModState {
    /// The entry's tree has a fault no version of the mod could load (see `StateTree::check`).
    /// A restore that fails on such an entry does not keep it for later saves.
    #[must_use]
    pub fn is_corrupt(&self) -> bool {
        self.tree.structural_fault().is_some()
    }

    /// Checks an entry from a save file against `limits`.
    pub fn check(&self, limits: &LuaLimits) -> Result<(), ModError> {
        self.tree.check(limits)?;
        let bytes = encoded_size(self)?;
        if bytes > limits.max_state_bytes {
            return Err(too_large(format!(
                "the saved state is {bytes} bytes, more than the {} allowed",
                limits.max_state_bytes
            )));
        }
        if self.entities.len() > limits.max_state_entities {
            return Err(too_large(format!(
                "the saved state lists {} entities, more than the {} allowed",
                self.entities.len(),
                limits.max_state_entities
            )));
        }
        let bad_entity = self.entities.iter().any(|e| !e.restorable());
        if bad_entity {
            return Err(state_error("the saved state lists a bad entity"));
        }
        Ok(())
    }
}

/// Reads the version a `regia.state_version` value names; absent is 1.
pub(crate) fn state_version_of(value: &Value) -> Result<u32, ModError> {
    let number = match value {
        Value::Nil => return Ok(1),
        Value::Integer(i) => *i as f64,
        Value::Number(n) => *n,
        other => {
            return Err(ModError::new(
                LuaErrorCode::BadValue,
                format!("regia.state_version must be a number, not a {}", lua_type(other)),
            ));
        }
    };
    if number.fract() != 0.0 || !(1.0..=f64::from(u32::MAX)).contains(&number) {
        return Err(ModError::new(
            LuaErrorCode::BadValue,
            format!("regia.state_version must be a whole number from 1, not {number}"),
        ));
    }
    Ok(number as u32)
}

/// Frames `file`: magic, version (`u32` LE), postcard payload.
pub(crate) fn encode(file: &LuaStateFile) -> Result<Vec<u8>, ModError> {
    let payload = postcard::to_allocvec(file).map_err(|e| state_error(e.to_string()))?;
    let mut bytes = Vec::with_capacity(payload.len() + 8);
    bytes.extend_from_slice(LUASTATE_MAGIC);
    bytes.extend_from_slice(&LUASTATE_FORMAT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&payload);
    Ok(bytes)
}

/// The encoded size of one mod's entry.
pub(crate) fn encoded_size(state: &ModState) -> Result<u64, ModError> {
    postcard::experimental::serialized_size(state)
        .map(|n| n as u64)
        .map_err(|e| state_error(e.to_string()))
}

/// Reads a `luastate` blob. The bytes come from a save file: nothing in them is trusted, and each
/// entry still goes through [`ModState::check`] before it is used.
pub fn decode(bytes: &[u8], limits: &LuaLimits) -> Result<LuaStateFile, ModError> {
    if bytes.len() as u64 > limits.max_state_total_bytes.saturating_add(CONTAINER_BYTES) {
        return Err(too_large(format!(
            "the saved Lua state is {} bytes, more than the {} allowed",
            bytes.len(),
            limits.max_state_total_bytes
        )));
    }
    let Some((header, payload)) = bytes.split_first_chunk::<8>() else {
        return Err(state_error("the saved Lua state is too short"));
    };
    let (magic, version) = header.split_at(4);
    if magic != LUASTATE_MAGIC {
        return Err(state_error("the saved Lua state has the wrong magic"));
    }
    let version = version
        .first_chunk::<4>()
        .map_or(0, |version| u32::from_le_bytes(*version));
    if version == 0 {
        return Err(state_error("the saved Lua state has version 0"));
    }
    if version > LUASTATE_FORMAT_VERSION {
        return Err(state_error(format!(
            "the saved Lua state is version {version}, newer than this engine reads ({LUASTATE_FORMAT_VERSION})"
        )));
    }
    postcard::from_bytes(payload)
        .map_err(|e| state_error(format!("the saved Lua state does not decode: {e}")))
}

/// The `from` versions of the registered migration steps (none: v1 is the first version).
#[must_use]
pub fn luastate_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde field-list fingerprint of `luastate` (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn luastate_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<LuaStateFile>(),
        serde_field_names::<ModState>(),
        serde_field_names::<StateTree>(),
        serde_field_names::<SavedModEntity>(),
        serde_field_names::<Rng>(),
        serde_variant_names::<StateKey>(),
        serde_variant_names::<StateValue>(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua() -> Lua {
        Lua::new()
    }

    fn capture(lua: &Lua, source: &str) -> Result<StateTree, ModError> {
        let table: Table = lua.load(source).eval().expect("eval");
        StateTree::capture(&table, &LuaLimits::default())
    }

    /// Sec review (ADR-5870 § 2): the cap looked at the parent path only, so a 600 KB key made a
    /// 600 KB path that each of its child tables copied (60 GB for 100,000 children on every save,
    /// peek and reload, in host code the budget does not see).
    #[test]
    fn a_long_key_makes_a_short_path() {
        let long = StateKey::String(vec![b'k'; 600_000]);
        let path = child_path("regia.state", &long);
        assert!(path.len() <= MAX_PATH, "{} bytes", path.len());
        assert!(path.starts_with("regia.state.kkk"));
        // Its children stay as short.
        assert!(child_path(&path, &long).len() <= MAX_PATH);
        assert!(child_path(&path, &StateKey::Number(1.0)).len() <= MAX_PATH);
        // A cut never splits a character.
        let wide = StateKey::String("é".repeat(400).into_bytes());
        let path = child_path("regia.state.a", &wide);
        assert!(path.len() <= MAX_PATH);
        assert!(path.ends_with('é') || path.ends_with('\u{FFFD}'), "{path}");
        // A short key is kept whole.
        assert_eq!(
            child_path("regia.state", &StateKey::String(b"hp".to_vec())),
            "regia.state.hp"
        );
    }

    /// The same, end to end: capturing 2,000 child tables under a 100 KB key copies paths of at
    /// most `MAX_PATH` bytes, and an error under them names a short path.
    #[test]
    fn an_error_under_a_long_key_names_a_short_path() {
        let lua = lua();
        let error = capture(
            &lua,
            r#"local t = {} for i = 1, 2000 do t[i] = {} end
               t[2000] = { f = function() end }
               return { [string.rep("k", 100000)] = t }"#,
        )
        .expect_err("a function cannot be saved");
        assert!(
            error.message.len() < MAX_PATH + 200,
            "{} bytes",
            error.message.len()
        );
    }

    /// Re-captures a built tree: what the mod would see, as data.
    fn round_trip(lua: &Lua, tree: &StateTree) -> StateTree {
        let built = tree.build(lua).expect("build");
        StateTree::capture(&built, &LuaLimits::default()).expect("capture")
    }

    #[test]
    fn keys_of_every_kind_binary_strings_and_vectors_round_trip() {
        let lua = lua();
        lua.globals()
            .set(
                "vec",
                lua.create_function(|_, (x, y, z): (f32, f32, f32)| {
                    Ok(LuaVec3(regia_core::math::Vec3::new(x, y, z)))
                })
                .expect("fn"),
            )
            .expect("set");
        let tree = capture(
            &lua,
            r#"return {
                1, 2.5, [10] = "ten", [-3] = true, [0.5] = false,
                ["10"] = "the string key", [true] = "yes",
                bytes = "\0\255\254binary", huge = math.huge, tiny = -math.huge,
                position = vec(1, 2, 3), nested = { deep = { deeper = {} } },
            }"#,
        )
        .expect("plain");
        assert_eq!(round_trip(&lua, &tree), tree);
        let root = &tree.tables[0];
        let string_key = (
            StateKey::String(b"bytes".to_vec()),
            StateValue::String(b"\0\xff\xfebinary".to_vec()),
        );
        assert!(root.contains(&string_key), "bytes survive: {root:?}");
        // Number 10 and string "10" stay two keys.
        assert!(root.iter().any(|(k, _)| *k == StateKey::Number(10.0)));
        assert!(root.iter().any(|(k, _)| *k == StateKey::String(b"10".to_vec())));
    }

    #[test]
    fn shared_tables_and_cycles_keep_their_identity() {
        let lua = lua();
        let tree = capture(
            &lua,
            "local shared = { n = 1 } local t = { a = shared, b = shared } t.me = t shared.up = t return t",
        )
        .expect("plain");
        assert_eq!(tree.tables.len(), 2, "two tables, not a copy per reference");
        let built = tree.build(&lua).expect("build");
        lua.globals().set("t", built).expect("set");
        let same: bool = lua
            .load("t.a.n = 2 return t.b.n == 2 and t.me == t and t.a.up == t")
            .eval()
            .expect("eval");
        assert!(same);
    }

    #[test]
    fn the_same_data_captures_the_same_whatever_the_insertion_order() {
        let lua = lua();
        let a = capture(&lua, "local t = {} t.a = {x=1} t.b = {y=2} t[1] = {z=3} return t").expect("plain");
        let b = capture(
            &lua,
            "local t = {} t[1] = {z=3} t.junk = 1 t.b = {y=2} t.a = {x=1} t.junk = nil return t",
        )
        .expect("plain");
        assert_eq!(a, b);
    }

    #[test]
    fn non_plain_values_are_refused_with_their_path() {
        let lua = lua();
        for (source, path) in [
            ("return { f = function() end }", "regia.state.f"),
            (
                "return { co = coroutine.create(function() end) }",
                "regia.state.co",
            ),
            ("return { a = { b = { print } } }", "regia.state.a.b[1]"),
            ("return { [{}] = 1 }", "regia.state"),
            ("return { [print] = 1 }", "regia.state"),
            ("return { o = setmetatable({}, {}) }", "regia.state.o"),
            ("return setmetatable({}, { __index = print })", "regia.state"),
        ] {
            let error = capture(&lua, source).expect_err(source);
            assert_eq!(error.code, LuaErrorCode::BadValue, "{source}");
            assert!(error.message.contains(path), "{source}: {}", error.message);
        }
    }

    #[test]
    fn caps_refuse_large_and_deep_states() {
        let lua = lua();
        let limits = LuaLimits {
            max_state_items: 100,
            max_state_bytes: 1000,
            ..LuaLimits::default()
        };
        let table: Table = lua
            .load("local t = {} for i = 1, 200 do t[i] = i end return t")
            .eval()
            .expect("eval");
        let error = StateTree::capture(&table, &limits).expect_err("items");
        assert_eq!(error.code, LuaErrorCode::TooLarge);
        let table: Table = lua.load("return { string.rep('x', 2000) }").eval().expect("eval");
        let error = StateTree::capture(&table, &limits).expect_err("bytes");
        assert_eq!(error.code, LuaErrorCode::TooLarge);
        let table: Table = lua
            .load("local t = {} local c = t for i = 1, 200 do c.n = {} c = c.n end return t")
            .eval()
            .expect("eval");
        let error = StateTree::capture(&table, &LuaLimits::default()).expect_err("depth");
        assert_eq!(error.code, LuaErrorCode::BadValue);
    }

    fn chain(tables: usize) -> StateTree {
        StateTree {
            tables: (0..tables)
                .map(|i| {
                    if i + 1 < tables {
                        vec![(StateKey::String(b"n".to_vec()), StateValue::Table((i + 1) as u32))]
                    } else {
                        Vec::new()
                    }
                })
                .collect(),
        }
    }

    /// Audit wave 5 (luasave Low): loading enforces the capture's nesting limit. The deepest
    /// chain a capture writes passes `check`, one level more is refused, and a back edge to the
    /// root (a cycle) does not count as nesting.
    #[test]
    fn a_loaded_state_nested_past_the_capture_limit_is_refused() {
        let lua = lua();
        let limits = LuaLimits::default();
        let deepest: Table = lua
            .load(format!(
                "local t = {{}} local c = t for i = 1, {} do c.n = {{}} c = c.n end return t",
                MAX_STATE_DEPTH - 1
            ))
            .eval()
            .expect("eval");
        let captured = StateTree::capture(&deepest, &limits).expect("capture");
        assert!(captured.check(&limits).is_ok());
        assert!(chain(MAX_STATE_DEPTH).check(&limits).is_ok());
        let error = chain(MAX_STATE_DEPTH + 1).check(&limits).expect_err("too deep");
        assert_eq!(error.code, LuaErrorCode::State);
        let mut cycle = chain(4);
        cycle.tables[3].push((StateKey::String(b"up".to_vec()), StateValue::Table(0)));
        assert!(cycle.check(&limits).is_ok());
    }

    /// Audit wave 6 (luasave L1): `-0` and `+0` are one key in Lua; a saved table that lists
    /// both (or any key twice) is refused instead of one value vanishing on load. The faults no
    /// version of a mod could load mark the entry corrupt; a cap does not.
    #[test]
    fn a_key_listed_twice_under_lua_equality_is_refused() {
        let limits = LuaLimits::default();
        let tree = |keys: Vec<StateKey>| StateTree {
            tables: vec![keys.into_iter().map(|k| (k, StateValue::Bool(true))).collect()],
        };
        let zeros = tree(vec![StateKey::Number(-0.0), StateKey::Number(0.0)]);
        assert_eq!(
            zeros.check(&limits).expect_err("one key").code,
            LuaErrorCode::State
        );
        let twice = tree(vec![
            StateKey::String(b"a".to_vec()),
            StateKey::String(b"a".to_vec()),
        ]);
        assert!(twice.check(&limits).is_err());
        assert!(
            tree(vec![StateKey::Bool(true), StateKey::Bool(true)])
                .check(&limits)
                .is_err()
        );
        let distinct = tree(vec![
            StateKey::Number(0.0),
            StateKey::Number(1.0),
            StateKey::String(b"0".to_vec()),
            StateKey::Bool(false),
        ]);
        assert!(distinct.check(&limits).is_ok());
        // The same key in two tables is two keys.
        let mut two = chain(2);
        two.tables[1].push((StateKey::String(b"n".to_vec()), StateValue::Bool(true)));
        assert!(two.check(&limits).is_ok());
        let entry = |tree: StateTree| ModState {
            tree,
            ..sample().mods.remove(0)
        };
        assert!(entry(zeros).is_corrupt());
        assert!(entry(tree(vec![StateKey::Number(f64::NAN)])).is_corrupt());
        assert!(
            !entry(chain(MAX_STATE_DEPTH + 1)).is_corrupt(),
            "depth is a cap, not a fault"
        );
    }

    #[test]
    fn many_tables_stay_clear_of_the_reference_stack() {
        // Far more tables than the binding's reference stack can hold at once on Lua 5.1 (8,000).
        let lua = lua();
        let tree =
            capture(&lua, "local t = {} for i = 1, 30000 do t[i] = { i } end return t").expect("plain");
        assert_eq!(tree.tables.len(), 30_001);
        assert_eq!(round_trip(&lua, &tree), tree);
    }

    fn sample() -> LuaStateFile {
        LuaStateFile {
            mods: vec![ModState {
                id: "a".into(),
                mod_version: "1.0.0".into(),
                state_version: 3,
                rng: Rng::with_stream(7, 0),
                tree: StateTree {
                    tables: vec![
                        vec![
                            (StateKey::Number(1.0), StateValue::Table(1)),
                            (StateKey::String(b"x".to_vec()), StateValue::Vec3([1.0, 2.0, 3.0])),
                        ],
                        vec![(StateKey::Bool(true), StateValue::String(vec![0, 255]))],
                    ],
                },
                entities: vec![SavedModEntity {
                    name: "lamp".into(),
                    position: [0.0, 1.0, 0.0],
                }],
            }],
        }
    }

    #[test]
    fn the_container_round_trips_and_refuses_what_it_cannot_read() {
        let limits = LuaLimits::default();
        let file = sample();
        let bytes = encode(&file).expect("encode");
        assert_eq!(&bytes[..4], LUASTATE_MAGIC);
        assert_eq!(decode(&bytes, &limits).expect("decode"), file);
        assert_eq!(encode(&file).expect("encode"), bytes, "stable bytes");

        let code = |bytes: &[u8]| decode(bytes, &limits).map(|_| ()).map_err(|e| e.code);
        assert_eq!(code(b"RLS"), Err(LuaErrorCode::State));
        let mut wrong = bytes.clone();
        wrong[0] = b'X';
        assert_eq!(code(&wrong), Err(LuaErrorCode::State));
        let mut newer = bytes.clone();
        newer[4..8].copy_from_slice(&(LUASTATE_FORMAT_VERSION + 1).to_le_bytes());
        let error = decode(&newer, &limits).expect_err("newer");
        assert!(error.message.contains("newer"), "{}", error.message);
        let mut zero = bytes.clone();
        zero[4..8].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(code(&zero), Err(LuaErrorCode::State));
        assert_eq!(code(&bytes[..bytes.len() - 3]), Err(LuaErrorCode::State));
        let tiny = LuaLimits {
            max_state_total_bytes: 4,
            ..limits
        };
        assert_eq!(
            decode(&bytes, &tiny).map(|_| ()).map_err(|e| e.code),
            Err(LuaErrorCode::TooLarge)
        );
    }

    #[test]
    fn a_tree_from_a_file_is_checked_before_use() {
        let limits = LuaLimits::default();
        let mut entry = sample().mods.remove(0);
        entry.check(&limits).expect("sound");
        entry.tree.tables[1].push((StateKey::Bool(false), StateValue::Table(9)));
        assert_eq!(entry.check(&limits).map_err(|e| e.code), Err(LuaErrorCode::State));
        entry.tree.tables[1].pop();
        entry.tree.tables[1].push((StateKey::Number(f64::NAN), StateValue::Bool(true)));
        assert_eq!(entry.check(&limits).map_err(|e| e.code), Err(LuaErrorCode::State));
        entry.tree.tables[1].pop();
        entry.entities[0].position[0] = f32::NAN;
        assert_eq!(entry.check(&limits).map_err(|e| e.code), Err(LuaErrorCode::State));
        entry.entities[0].position[0] = 0.0;
        let few = LuaLimits {
            max_state_entities: 0,
            ..limits
        };
        assert_eq!(entry.check(&few).map_err(|e| e.code), Err(LuaErrorCode::TooLarge));
        entry.tree.tables.clear();
        assert_eq!(entry.check(&limits).map_err(|e| e.code), Err(LuaErrorCode::State));
    }

    #[test]
    fn hostile_bytes_never_panic_the_decoder() {
        // A fixed-seed PCG32: every run tries the same inputs.
        let limits = LuaLimits::default();
        let mut rng = Rng::with_stream(0x1919, 1);
        let valid = encode(&sample()).expect("encode");
        let lua = lua();
        for round in 0..20_000u32 {
            let mut bytes = valid.clone();
            if round % 2 == 0 {
                for _ in 0..=(rng.next_u32() % 8) {
                    let at = 8 + (rng.next_u32() as usize % (bytes.len() - 8));
                    bytes[at] = rng.next_u32() as u8;
                }
            } else {
                bytes.truncate(8);
                bytes.extend((0..rng.next_u32() % 256).map(|_| rng.next_u32() as u8));
            }
            if let Ok(file) = decode(&bytes, &limits) {
                for entry in &file.mods {
                    if entry.check(&limits).is_ok() {
                        let _ = entry.tree.build(&lua);
                    }
                }
            }
        }
    }

    #[test]
    fn state_versions_are_whole_numbers_from_one() {
        assert_eq!(state_version_of(&Value::Nil), Ok(1));
        assert_eq!(state_version_of(&Value::Integer(4)), Ok(4));
        assert_eq!(state_version_of(&Value::Number(4.0)), Ok(4));
        for bad in [
            Value::Integer(0),
            Value::Integer(-1),
            Value::Number(1.5),
            Value::Number(f64::NAN),
            Value::Number(1e12),
            Value::Boolean(true),
        ] {
            assert_eq!(
                state_version_of(&bad).map_err(|e| e.code),
                Err(LuaErrorCode::BadValue),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn json_shows_sequences_objects_vectors_and_references() {
        let tree = StateTree {
            tables: vec![
                vec![
                    (StateKey::String(b"list".to_vec()), StateValue::Table(1)),
                    (StateKey::String(b"me".to_vec()), StateValue::Table(0)),
                    (StateKey::String(b"v".to_vec()), StateValue::Vec3([1.0, 0.0, 0.0])),
                ],
                vec![
                    (StateKey::Number(1.0), StateValue::Number(5.0)),
                    (StateKey::Number(2.0), StateValue::Bool(true)),
                ],
            ],
        };
        assert_eq!(
            tree.to_json(),
            json!({ "list": [5.0, true], "me": { "ref": 0 }, "v": { "vec3": [1.0, 0.0, 0.0] } })
        );
    }

    #[test]
    fn the_fingerprint_names_every_layer() {
        let fp = luastate_schema_fingerprint();
        for field in [
            "mods",
            "state_version",
            "rng",
            "tables",
            "position",
            "Vec3",
            "Bool",
        ] {
            assert!(fp.contains(field), "{fp}");
        }
        assert!(luastate_migration_sources().is_empty());
    }
}
