// SPDX-License-Identifier: MPL-2.0
//! Values crossing the Lua boundary: [`ScriptValue`], the `vec3` and entity userdata.

use std::collections::BTreeMap;

use mlua::{AnyUserData, Lua, MetaMethod, UserData, UserDataFields, UserDataMethods, Value};
use regia_core::math::Vec3;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{HostError, LuaErrorCode};

/// Deepest nesting of lists and maps in one value.
pub const MAX_DEPTH: usize = 16;
/// Most items (every list element, map entry and leaf) in one value.
pub const MAX_ITEMS: usize = 4096;
/// Longest string in a value, in bytes.
pub const MAX_STRING_BYTES: usize = 64 * 1024;

/// Most bytes of strings and keys in one value. A Lua string is interned, so a table of references
/// to one long string costs a mod little: without this, reading it onto the Rust heap could take
/// [`MAX_ITEMS`] times [`MAX_STRING_BYTES`].
pub const MAX_VALUE_BYTES: usize = 1024 * 1024;

/// A plain value passed between the game and a mod (event payloads, `lua.run` results).
/// Maps are sorted by key, so a value converts to the same Lua table and JSON every time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum ScriptValue {
    /// `nil`.
    #[default]
    Nil,
    /// A boolean.
    Bool(bool),
    /// A number (Lua 5.1 numbers are doubles).
    Number(f64),
    /// A UTF-8 string (invalid bytes from Lua are replaced).
    String(String),
    /// A `regia.vec3`.
    Vec3([f32; 3]),
    /// An entity handle (ECS entity bits).
    Entity(u64),
    /// A Lua sequence `{a, b, c}`.
    List(Vec<ScriptValue>),
    /// A Lua table with string (or number) keys.
    Map(BTreeMap<String, ScriptValue>),
}

impl ScriptValue {
    /// The value as JSON for the agent protocol: `null`, booleans, numbers, strings, arrays,
    /// objects; a vector is `{ "vec3": [x, y, z] }` and an entity `{ "entity": bits }`.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            ScriptValue::Nil => serde_json::Value::Null,
            ScriptValue::Bool(b) => json!(b),
            ScriptValue::Number(n) => json!(n),
            ScriptValue::String(s) => json!(s),
            ScriptValue::Vec3(v) => json!({ "vec3": v }),
            ScriptValue::Entity(bits) => json!({ "entity": bits }),
            ScriptValue::List(items) => serde_json::Value::Array(items.iter().map(Self::to_json).collect()),
            ScriptValue::Map(map) => {
                serde_json::Value::Object(map.iter().map(|(k, v)| (k.clone(), v.to_json())).collect())
            }
        }
    }

    /// Converts a Lua value, enforcing [`MAX_DEPTH`], [`MAX_ITEMS`], [`MAX_STRING_BYTES`] and
    /// [`MAX_VALUE_BYTES`]. Functions, coroutines and foreign userdata are refused
    /// (`lua_bad_value`).
    pub(crate) fn from_lua(value: &Value) -> mlua::Result<Self> {
        from_lua_inner(value, 0, &mut Walk::default())
    }

    /// About how many bytes the value takes on the Rust heap.
    pub(crate) fn weight(&self) -> u64 {
        const ITEM: u64 = std::mem::size_of::<ScriptValue>() as u64;
        match self {
            ScriptValue::String(s) => ITEM + s.len() as u64,
            ScriptValue::List(items) => ITEM + items.iter().map(Self::weight).sum::<u64>(),
            ScriptValue::Map(map) => ITEM + map.iter().map(|(k, v)| k.len() as u64 + v.weight()).sum::<u64>(),
            _ => ITEM,
        }
    }

    /// Builds the Lua value.
    pub(crate) fn to_lua(&self, lua: &Lua) -> mlua::Result<Value> {
        Ok(match self {
            ScriptValue::Nil => Value::Nil,
            ScriptValue::Bool(b) => Value::Boolean(*b),
            ScriptValue::Number(n) => Value::Number(*n),
            ScriptValue::String(s) => Value::String(lua.create_string(s)?),
            ScriptValue::Vec3(v) => counted_vec3(lua, Vec3::from_array(*v))?,
            ScriptValue::Entity(bits) => entity_value(lua, *bits)?,
            ScriptValue::List(items) => {
                let table = lua.create_table_with_capacity(items.len(), 0)?;
                for (i, item) in items.iter().enumerate() {
                    table.raw_set(i + 1, item.to_lua(lua)?)?;
                }
                Value::Table(table)
            }
            ScriptValue::Map(map) => {
                let table = lua.create_table_with_capacity(0, map.len())?;
                for (k, v) in map {
                    table.raw_set(k.as_str(), v.to_lua(lua)?)?;
                }
                Value::Table(table)
            }
        })
    }
}

/// A short description of `value` for error messages (`nil`, `a list`, ...).
pub(crate) fn describe(value: &ScriptValue) -> String {
    match value {
        ScriptValue::Nil => "nil".to_owned(),
        ScriptValue::Bool(_) => "a bool".to_owned(),
        ScriptValue::Number(_) => "a number".to_owned(),
        ScriptValue::String(_) => "a string".to_owned(),
        ScriptValue::Vec3(_) => "a vec3".to_owned(),
        ScriptValue::Entity(_) => "an entity".to_owned(),
        ScriptValue::List(_) => "a list".to_owned(),
        ScriptValue::Map(_) => "a map".to_owned(),
    }
}

/// The name Lua 5.1's `type()` gives `value`: the binding reads whole numbers as integers, but
/// Lua 5.1 has one number type, so both are `number`.
pub(crate) fn lua_type(value: &Value) -> &'static str {
    match value {
        Value::Integer(_) | Value::Number(_) => "number",
        other => other.type_name(),
    }
}

fn bad(message: impl Into<String>) -> mlua::Error {
    HostError::lua(LuaErrorCode::BadValue, message)
}

/// What one conversion has taken so far.
#[derive(Default)]
struct Walk {
    items: usize,
    bytes: usize,
}

impl Walk {
    fn count(&mut self) -> mlua::Result<()> {
        self.items += 1;
        if self.items > MAX_ITEMS {
            return Err(bad(format!("a value may hold at most {MAX_ITEMS} items")));
        }
        Ok(())
    }

    fn string(&mut self, s: &mlua::LuaString) -> mlua::Result<String> {
        let bytes = s.as_bytes();
        if bytes.len() > MAX_STRING_BYTES {
            return Err(bad(format!(
                "strings in values are at most {MAX_STRING_BYTES} bytes"
            )));
        }
        self.bytes += bytes.len();
        if self.bytes > MAX_VALUE_BYTES {
            return Err(bad(format!("a value may hold at most {MAX_VALUE_BYTES} bytes")));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn from_lua_inner(value: &Value, depth: usize, walk: &mut Walk) -> mlua::Result<ScriptValue> {
    walk.count()?;
    Ok(match value {
        Value::Nil => ScriptValue::Nil,
        Value::Boolean(b) => ScriptValue::Bool(*b),
        Value::Integer(i) => ScriptValue::Number(*i as f64),
        Value::Number(n) => ScriptValue::Number(*n),
        Value::String(s) => ScriptValue::String(walk.string(s)?),
        Value::UserData(ud) => userdata_value(ud)?,
        Value::Table(table) => {
            if depth >= MAX_DEPTH {
                return Err(bad(format!("values nest at most {MAX_DEPTH} levels")));
            }
            let len = table.raw_len();
            // Each value converts while the walk is on it, so no level keeps its Lua strings
            // alive during the next one: the binding's reference stack is small on Lua 5.1.
            let mut entries: Vec<(MapKey, ScriptValue)> = Vec::new();
            for pair in table.pairs::<Value, Value>() {
                let (key, value) = pair?;
                if entries.len() >= MAX_ITEMS {
                    return Err(bad(format!("a value may hold at most {MAX_ITEMS} items")));
                }
                let key = match &key {
                    Value::String(s) => MapKey::String(walk.string(s)?),
                    Value::Integer(i) => MapKey::Number(*i as f64),
                    Value::Number(n) => MapKey::Number(*n),
                    other => {
                        return Err(bad(format!(
                            "map keys must be strings or numbers, not {}",
                            lua_type(other)
                        )));
                    }
                };
                entries.push((key, from_lua_inner(&value, depth + 1, walk)?));
            }
            let is_list = len > 0
                && entries.len() == len
                && entries
                    .iter()
                    .all(|(k, _)| k.index().is_some_and(|i| i >= 1 && i <= len));
            if is_list {
                let mut list = vec![ScriptValue::Nil; len];
                for (key, value) in entries {
                    if let Some(i) = key.index() {
                        list[i - 1] = value;
                    }
                }
                ScriptValue::List(list)
            } else {
                // Lua keeps a number key and its text form apart (`{[1] = .., ["1"] = ..}`); a
                // string-keyed map cannot, so the value is refused rather than dropping a field.
                let mut map = BTreeMap::new();
                for (key, value) in entries {
                    if map.insert(key.into_text(), value).is_some() {
                        return Err(bad(
                            "map keys collide when written as text (a number key and its text form)",
                        ));
                    }
                }
                ScriptValue::Map(map)
            }
        }
        other => return Err(bad(format!("a {} cannot leave a mod", lua_type(other)))),
    })
}

/// A table key while a value is converted.
enum MapKey {
    String(String),
    Number(f64),
}

impl MapKey {
    /// The list index this key names.
    fn index(&self) -> Option<usize> {
        match self {
            MapKey::Number(n) if n.fract() == 0.0 && *n >= 1.0 && *n < 9.0e15 => Some(*n as usize),
            _ => None,
        }
    }

    /// The key of a map entry: a number as text.
    fn into_text(self) -> String {
        match self {
            MapKey::String(s) => s,
            MapKey::Number(n) if n.fract() == 0.0 && n.abs() < 9.0e15 => format!("{}", n as i64),
            MapKey::Number(n) => n.to_string(),
        }
    }
}

fn userdata_value(ud: &AnyUserData) -> mlua::Result<ScriptValue> {
    if let Ok(v) = ud.borrow::<LuaVec3>() {
        return Ok(ScriptValue::Vec3(v.0.to_array()));
    }
    if let Ok(e) = ud.borrow::<EntityHandle>() {
        return Ok(ScriptValue::Entity(e.0));
    }
    Err(bad("this userdata cannot leave a mod"))
}

/// `regia.vec3` userdata values [`counted_vec3`] has built in this process.
static VEC3_USERDATA_CREATES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many `regia.vec3` userdata values this process has built.
///
/// Each one is a fresh userdata. A stored vector and an upvalue keep their own value, so this
/// count is not a pool: recycling one userdata would change every Lua reference that holds it.
#[must_use]
pub fn vec3_userdata_creates() -> u64 {
    VEC3_USERDATA_CREATES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Zeroes [`vec3_userdata_creates`].
pub fn reset_vec3_userdata_creates() {
    VEC3_USERDATA_CREATES.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// Builds one immutable `regia.vec3` userdata and counts it.
pub(crate) fn counted_vec3(lua: &Lua, v: Vec3) -> mlua::Result<Value> {
    VEC3_USERDATA_CREATES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(Value::UserData(lua.create_userdata(LuaVec3(v))?))
}

/// `regia.vec3`: an immutable 3-vector.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LuaVec3(pub(crate) Vec3);

/// Reads a `vec3` argument.
pub(crate) fn vec3_arg(value: &Value) -> mlua::Result<Vec3> {
    match value {
        Value::UserData(ud) => ud
            .borrow::<LuaVec3>()
            .map(|v| v.0)
            .map_err(|_| bad("expected a vec3")),
        other => Err(bad(format!("expected a vec3, got {}", lua_type(other)))),
    }
}

fn number_arg(value: &Value) -> Option<f32> {
    match value {
        Value::Integer(i) => Some(*i as f32),
        Value::Number(n) => Some(*n as f32),
        _ => None,
    }
}

impl UserData for LuaVec3 {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("x", |_, this| Ok(this.0.x));
        fields.add_field_method_get("y", |_, this| Ok(this.0.y));
        fields.add_field_method_get("z", |_, this| Ok(this.0.z));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("length", |_, this, ()| Ok(this.0.length()));
        methods.add_method("dot", |_, this, other: Value| Ok(this.0.dot(vec3_arg(&other)?)));
        methods.add_method("cross", |lua, this, other: Value| {
            counted_vec3(lua, this.0.cross(vec3_arg(&other)?))
        });
        methods.add_method("normalized", |lua, this, ()| {
            counted_vec3(lua, this.0.normalize_or_zero())
        });
        methods.add_meta_function(MetaMethod::Add, |lua, (a, b): (Value, Value)| {
            counted_vec3(lua, vec3_arg(&a)? + vec3_arg(&b)?)
        });
        methods.add_meta_function(MetaMethod::Sub, |lua, (a, b): (Value, Value)| {
            counted_vec3(lua, vec3_arg(&a)? - vec3_arg(&b)?)
        });
        methods.add_meta_function(MetaMethod::Mul, |lua, (a, b): (Value, Value)| {
            let v = match (number_arg(&a), number_arg(&b)) {
                (Some(s), None) => vec3_arg(&b)? * s,
                (None, Some(s)) => vec3_arg(&a)? * s,
                _ => vec3_arg(&a)? * vec3_arg(&b)?,
            };
            counted_vec3(lua, v)
        });
        methods.add_meta_function(MetaMethod::Div, |lua, (a, b): (Value, Value)| {
            let v = match number_arg(&b) {
                Some(s) => vec3_arg(&a)? / s,
                None => vec3_arg(&a)? / vec3_arg(&b)?,
            };
            counted_vec3(lua, v)
        });
        methods.add_meta_method(MetaMethod::Unm, |lua, this, ()| counted_vec3(lua, -this.0));
        methods.add_meta_function(MetaMethod::Eq, |_, (a, b): (Value, Value)| {
            Ok(matches!((vec3_arg(&a), vec3_arg(&b)), (Ok(x), Ok(y)) if x == y))
        });
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!("vec3({}, {}, {})", this.0.x, this.0.y, this.0.z))
        });
    }
}

/// Registry table of entity-handle userdata, keyed by index then generation.
const HANDLES_KEY: &str = "regia.handles";

/// The userdata for entity `bits`, reused when this state already handed one out.
///
/// Lua 5.1 table keys compare userdata by identity. Two `regia.find` results for the same
/// entity are therefore the same key. The index is the low 32 bits and the generation the
/// high 32, so neither key needs more precision than a Lua number holds.
pub(crate) fn entity_value(lua: &Lua, bits: u64) -> mlua::Result<Value> {
    let index = i64::from((bits & 0xffff_ffff) as u32);
    let generation = i64::from((bits >> 32) as u32);
    let table = match lua.named_registry_value::<Value>(HANDLES_KEY)? {
        Value::Table(table) => table,
        _ => {
            let table = lua.create_table()?;
            lua.set_named_registry_value(HANDLES_KEY, table.clone())?;
            table
        }
    };
    let bucket = match table.raw_get::<Value>(index)? {
        Value::Table(bucket) => bucket,
        _ => {
            let bucket = lua.create_table()?;
            // Weak values: the cache keeps a handle only while the script holds it. A strong
            // bucket kept every handle ever handed out, one per entity generation, for the
            // state's life, and that memory counts against the mod's limit. The outer table
            // stays strong: it is bounded by the highest entity index, which the ECS reuses.
            bucket.set_metatable(Some(weak_values(lua)?))?;
            table.raw_set(index, bucket.clone())?;
            bucket
        }
    };
    if let Value::UserData(existing) = bucket.raw_get::<Value>(generation)? {
        return Ok(Value::UserData(existing));
    }
    let userdata = lua.create_userdata(EntityHandle(bits))?;
    bucket.raw_set(generation, userdata.clone())?;
    Ok(Value::UserData(userdata))
}

/// Registry key of the shared `{ __mode = "v" }` metatable of the handle buckets.
const WEAK_VALUES_KEY: &str = "regia.weak_values";

/// The shared weak-valued metatable [`entity_value`] puts on each bucket.
fn weak_values(lua: &Lua) -> mlua::Result<mlua::Table> {
    if let Value::Table(meta) = lua.named_registry_value::<Value>(WEAK_VALUES_KEY)? {
        return Ok(meta);
    }
    let meta = lua.create_table()?;
    meta.raw_set("__mode", "v")?;
    lua.set_named_registry_value(WEAK_VALUES_KEY, meta.clone())?;
    Ok(meta)
}

/// An entity handle: the ECS entity bits. Methods go through the call's world dispatcher
/// (`host::world_call`), so a handle kept past its entity's life answers `nil`/`false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EntityHandle(pub(crate) u64);

/// Reads an entity handle argument.
pub(crate) fn entity_arg(value: &Value) -> mlua::Result<u64> {
    match value {
        Value::UserData(ud) => ud
            .borrow::<EntityHandle>()
            .map(|e| e.0)
            .map_err(|_| bad("expected an entity handle")),
        other => Err(bad(format!("expected an entity handle, got {}", lua_type(other)))),
    }
}

/// Reads a script-component name argument.
pub(crate) fn component_arg(value: &Value) -> mlua::Result<String> {
    match value {
        Value::String(s) if s.as_bytes().len() <= crate::components::MAX_NAME_BYTES => {
            Ok(String::from_utf8_lossy(&s.as_bytes()).into_owned())
        }
        Value::String(_) => Err(bad(format!(
            "a component name is at most {} bytes",
            crate::components::MAX_NAME_BYTES
        ))),
        other => Err(bad(format!("expected a component name, got {}", lua_type(other)))),
    }
}

impl UserData for EntityHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        use crate::host::{WorldOp, world_call, world_call2};
        methods.add_method("id", |_, this, ()| {
            // Index and generation as a string: the bits do not fit a Lua 5.1 number exactly.
            Ok(format!("{}v{}", this.0 & 0xffff_ffff, this.0 >> 32))
        });
        methods.add_method("alive", |lua, this, ()| {
            world_call(lua, WorldOp::Alive, this.0, Value::Nil)
        });
        methods.add_method("name", |lua, this, ()| {
            world_call(lua, WorldOp::Name, this.0, Value::Nil)
        });
        methods.add_method("position", |lua, this, ()| {
            world_call(lua, WorldOp::Position, this.0, Value::Nil)
        });
        methods.add_method("set_position", |lua, this, v: Value| {
            vec3_arg(&v)?;
            world_call(lua, WorldOp::SetPosition, this.0, v)
        });
        methods.add_method("despawn", |lua, this, ()| {
            world_call(lua, WorldOp::Despawn, this.0, Value::Nil)
        });
        // Script components (ADR-3356): `e:add("mover")`, `e:has`, `e:remove`, `e:props("mover")`,
        // `e:set_prop("mover", "speed", 3)`.
        methods.add_method("add", |lua, this, name: Value| {
            let name = component_arg(&name)?;
            world_call(
                lua,
                WorldOp::Attach,
                this.0,
                Value::String(lua.create_string(&name)?),
            )
        });
        methods.add_method("has", |lua, this, name: Value| {
            let name = component_arg(&name)?;
            world_call(
                lua,
                WorldOp::HasComponent,
                this.0,
                Value::String(lua.create_string(&name)?),
            )
        });
        methods.add_method("remove", |lua, this, name: Value| {
            let name = component_arg(&name)?;
            world_call(
                lua,
                WorldOp::RemoveComponent,
                this.0,
                Value::String(lua.create_string(&name)?),
            )
        });
        methods.add_method("props", |lua, this, name: Value| {
            let name = component_arg(&name)?;
            world_call(
                lua,
                WorldOp::GetProps,
                this.0,
                Value::String(lua.create_string(&name)?),
            )
        });
        methods.add_method(
            "set_prop",
            |lua, this, (name, key, value): (Value, Value, Value)| {
                let name = component_arg(&name)?;
                let key = component_arg(&key)?;
                let update = lua.create_table()?;
                update.raw_set(1, key)?;
                update.raw_set(2, value)?;
                world_call2(
                    lua,
                    WorldOp::SetProps,
                    this.0,
                    Value::String(lua.create_string(&name)?),
                    Value::Table(update),
                )
            },
        );
        methods.add_meta_function(MetaMethod::Eq, |_, (a, b): (Value, Value)| {
            Ok(matches!((entity_arg(&a), entity_arg(&b)), (Ok(x), Ok(y)) if x == y))
        });
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!("entity({}v{})", this.0 & 0xffff_ffff, this.0 >> 32))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua() -> Lua {
        Lua::new()
    }

    #[test]
    fn plain_values_round_trip_through_lua() {
        let lua = lua();
        let mut map = BTreeMap::new();
        map.insert("a".to_owned(), ScriptValue::Number(1.5));
        map.insert(
            "list".to_owned(),
            ScriptValue::List(vec![ScriptValue::Bool(true), ScriptValue::String("x".into())]),
        );
        map.insert("v".to_owned(), ScriptValue::Vec3([1.0, 2.0, 3.0]));
        map.insert("e".to_owned(), ScriptValue::Entity(42 | (7 << 32)));
        let value = ScriptValue::Map(map);
        let back = ScriptValue::from_lua(&value.to_lua(&lua).expect("to lua")).expect("from lua");
        assert_eq!(back, value);
    }

    #[test]
    fn functions_deep_trees_and_huge_tables_are_refused() {
        let lua = lua();
        let check = |src: &str| {
            let v: Value = lua.load(src).eval().expect("eval");
            let err = ScriptValue::from_lua(&v).expect_err(src);
            assert_eq!(crate::error::classify(&err), LuaErrorCode::BadValue, "{src}");
        };
        check("return function() end");
        check("return coroutine.create(function() end)");
        check("local t = {} local c = t for i = 1, 40 do c.next = {} c = c.next end return t");
        check("local t = {} t.self = t return t");
        check("local t = {} for i = 1, 5000 do t[i] = i end return t");
        check("return { [{}] = 1 }");
        check("return string.rep('x', 70000)");
    }

    #[test]
    fn mixed_tables_become_sorted_maps_and_sequences_lists() {
        let lua = lua();
        let v: Value = lua.load("return { 10, 20, 30 }").eval().expect("eval");
        assert_eq!(
            ScriptValue::from_lua(&v).expect("list"),
            ScriptValue::List(vec![
                ScriptValue::Number(10.0),
                ScriptValue::Number(20.0),
                ScriptValue::Number(30.0)
            ])
        );
        let v: Value = lua.load("return { 1, 2, b = 3, [10] = 4 }").eval().expect("eval");
        let ScriptValue::Map(map) = ScriptValue::from_lua(&v).expect("map") else {
            panic!("expected a map");
        };
        assert_eq!(map.keys().cloned().collect::<Vec<_>>(), ["1", "10", "2", "b"]);
        let v: Value = lua.load("return {}").eval().expect("eval");
        assert_eq!(
            ScriptValue::from_lua(&v).expect("empty"),
            ScriptValue::Map(BTreeMap::new())
        );
    }

    #[test]
    fn a_number_key_and_its_text_form_cannot_silently_merge() {
        let lua = lua();
        // Lua keeps `1` and `"1"` as two keys; the text forms collide in a string-keyed map,
        // so the value is refused instead of dropping one of the fields.
        let v: Value = lua
            .load(r#"return { [1] = "index", ["1"] = "name" }"#)
            .eval()
            .expect("eval");
        let err = ScriptValue::from_lua(&v).expect_err("the collision is refused");
        assert_eq!(crate::error::classify(&err), LuaErrorCode::BadValue);
        let v: Value = lua
            .load(r#"return { [1] = "index", ["2"] = "name" }"#)
            .eval()
            .expect("eval");
        let ScriptValue::Map(map) = ScriptValue::from_lua(&v).expect("distinct text forms merge") else {
            panic!("expected a map");
        };
        assert_eq!(map.keys().cloned().collect::<Vec<_>>(), ["1", "2"]);
    }

    /// Handles of entities the script no longer holds must not stay in the registry. Before the
    /// fix every generation of a reused entity slot kept its userdata for the state's life, so a
    /// mod that churns spawned entities grew towards its memory limit. A handle the script does
    /// hold keeps its identity (the same userdata, so it still works as a table key).
    #[test]
    fn entity_handles_the_script_dropped_are_collected_and_held_ones_keep_identity() {
        let lua = lua();
        let held = ScriptValue::Entity(7 | (1 << 32)).to_lua(&lua).expect("handle");
        lua.globals().set("held", held).expect("hold");
        for generation in 2..2_002u64 {
            let bits = 7 | (generation << 32);
            let _ = ScriptValue::Entity(bits).to_lua(&lua).expect("handle");
        }
        lua.gc_collect().expect("collect");
        lua.gc_collect().expect("collect");
        let registry: mlua::Table = lua.named_registry_value(HANDLES_KEY).expect("registry");
        let bucket: mlua::Table = registry.raw_get(7).expect("bucket for index 7");
        let live = bucket.pairs::<Value, Value>().count();
        assert_eq!(live, 1, "only the held handle stays cached, got {live}");
        let again = ScriptValue::Entity(7 | (1 << 32)).to_lua(&lua).expect("handle");
        let held: Value = lua.globals().get("held").expect("held");
        assert!(
            raw_equal(&again, &held),
            "a held handle is handed out again, not a copy"
        );
    }

    fn raw_equal(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::UserData(a), Value::UserData(b)) => a.to_pointer() == b.to_pointer(),
            _ => false,
        }
    }

    #[test]
    fn vec3_arithmetic() {
        let lua = lua();
        lua.globals()
            .set(
                "v",
                lua.create_function(|_, (x, y, z): (f32, f32, f32)| Ok(LuaVec3(Vec3::new(x, y, z))))
                    .expect("fn"),
            )
            .expect("set");
        let ok: bool = lua
            .load(
                "local a, b = v(1, 2, 3), v(4, 5, 6)
                 local c = (a + b) * 2 - v(0, 0, 18) / 2
                 return c == v(10, 14, 9) and (2 * a) == v(2, 4, 6) and a:dot(b) == 32
                    and a:cross(b) == v(-3, 6, -3) and (-a).x == -1 and v(3, 4, 0):length() == 5
                    and tostring(a) == 'vec3(1, 2, 3)' and v(0, 0, 0):normalized() == v(0, 0, 0)",
            )
            .eval()
            .expect("eval");
        assert!(ok);
    }

    #[test]
    fn json_shape() {
        let v = ScriptValue::List(vec![
            ScriptValue::Nil,
            ScriptValue::Vec3([1.0, 0.0, 0.0]),
            ScriptValue::Entity(5),
        ]);
        assert_eq!(
            v.to_json(),
            json!([null, { "vec3": [1.0, 0.0, 0.0] }, { "entity": 5 }])
        );
    }
}
