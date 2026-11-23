// SPDX-License-Identifier: MPL-2.0
//! Script components: typed per-entity properties a mod declares with `regia.component` and a
//! designer edits per entity in the editor inspector (ADR-3356, `docs/specs/84-lua-mods.md` § 13).
//!
//! A mod declares a component once, at load:
//!
//! ```lua
//! regia.component("mover", {
//!     props = {
//!         speed  = { type = "number", default = 2.0, min = 0.0, max = 20.0, step = 0.1 },
//!         active = { type = "bool",   default = true },
//!         label  = { type = "string", default = "mover" },
//!         offset = { type = "vec3",   default = regia.vec3(0, 1, 0) },
//!         tint   = { type = "color",  default = { 1.0, 0.5, 0.25, 1.0 } },
//!         target = { type = "entity" },                     -- default: none
//!         mesh   = { type = "asset",   filter = "mesh" },
//!         mode   = { type = "enum",    options = { "walk", "run" }, default = "walk" },
//!     },
//!     on_update = function(e, dt) local p = e:props() e:set_position(e:position() + p.offset * dt * p.speed) end,
//! })
//! ```
//!
//! Scripts attach it to entities with `e:add("mover")` (defaults applied), read values with
//! `e:props()` and write them with `e:set_prop(name, value)`. Every entity carrying the
//! declaration gets a [`LuaComponent`] whose values are edited per entity in the inspector and
//! saved in the scene like any reflected component. A hot reload reconciles stored values against
//! the new declaration: kept values survive, new props get their defaults, removed props go.

use std::collections::BTreeMap;

use mlua::{Table, Value};
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::value::ScriptValue;

/// Most properties one declaration may hold.
pub const MAX_PROPS_PER_DECL: usize = 64;
/// Most components one mod may declare.
pub const MAX_DECLS_PER_MOD: usize = 64;
/// Longest component or property name, in bytes.
pub const MAX_NAME_BYTES: usize = 64;
/// Most options an `enum` property offers, and the longest `options` list a declaration may pass
/// (repeated items count: the parse walks the whole list on the host, unseen by the budget).
pub const MAX_ENUM_OPTIONS: usize = 64;
/// Longest `enum` option, in bytes.
pub const MAX_ENUM_OPTION_BYTES: usize = 256;
/// Most bytes one declaration keeps on the Rust heap: property names, options, filters and
/// defaults. The host holds every declaration and copies it for the inspector and the agent, so
/// this, not the mod's Lua memory cap, bounds what a mod's declarations cost (ADR-5870).
pub const MAX_DECL_BYTES: u64 = 64 * 1024;

/// The value type of one declared property.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropType {
    /// A finite `f64`, edited with a drag widget, clamped to `min`/`max` when set.
    Number,
    /// A boolean checkbox.
    Bool,
    /// A string (one line unless `multiline`).
    String,
    /// A `regia.vec3`.
    Vec3,
    /// Linear RGBA, stored as a list of four numbers; edited with the colour widget.
    Color,
    /// An entity handle, or none. Edited in the inspector as a named reference.
    Entity,
    /// A project asset path, edited with the asset picker; `filter` restricts the choices.
    Asset,
    /// One of a fixed list of option strings; edited with a drop-down.
    Enum,
}

impl PropType {
    /// The name the declaration's `type` field takes.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::Bool => "bool",
            Self::String => "string",
            Self::Vec3 => "vec3",
            Self::Color => "color",
            Self::Entity => "entity",
            Self::Asset => "asset",
            Self::Enum => "enum",
        }
    }

    /// Parses the `type` field.
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "number" => Self::Number,
            "bool" => Self::Bool,
            "string" => Self::String,
            "vec3" => Self::Vec3,
            "color" => Self::Color,
            "entity" => Self::Entity,
            "asset" => Self::Asset,
            "enum" => Self::Enum,
            _ => return None,
        })
    }
}

/// One declared property: its type, default and editor hints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PropDecl {
    /// The property name (the key in the declaration's `props` table).
    pub name: String,
    /// The value type.
    pub ty: PropType,
    /// The value `e:add` applies and the inspector shows after a Reset. Resolved: never `Nil`
    /// except for an `entity` property with no reference.
    pub default: ScriptValue,
    /// The drag widget's lower bound, when set.
    pub min: Option<f64>,
    /// The drag widget's upper bound, when set.
    pub max: Option<f64>,
    /// The drag widget's step, when set.
    pub step: Option<f64>,
    /// The `enum` options, in declaration order.
    pub options: Vec<String>,
    /// The `asset` kind filter (globs or kind names, the inspector's `@AssetFilter` syntax).
    pub filter: Option<String>,
    /// A multiline editor for a `string`.
    pub multiline: bool,
}

impl PropDecl {
    /// About how many bytes the declaration keeps on the Rust heap ([`MAX_DECL_BYTES`]).
    fn weight(&self) -> u64 {
        let options: u64 = self.options.iter().map(|o| o.len() as u64).sum();
        (self.name.len() as u64)
            .saturating_add(options)
            .saturating_add(self.filter.as_ref().map_or(0, |f| f.len() as u64))
            .saturating_add(self.default.weight())
    }

    /// The value a fresh attachment starts with (the resolved `default`).
    #[must_use]
    pub fn initial(&self) -> ScriptValue {
        self.default.clone()
    }

    /// Checks and normalizes `value` for this property: the type must match, `nil` means the
    /// no-reference form of `entity`, numbers clamp to `min`/`max`, a colour is four finite
    /// channels, and an `enum` value must be one of `options`.
    ///
    /// # Errors
    /// Why the value does not fit, as a short sentence.
    pub fn coerce(&self, value: ScriptValue) -> Result<ScriptValue, String> {
        match (self.ty, value) {
            (_, ScriptValue::Nil) if self.ty == PropType::Entity => Ok(ScriptValue::Nil),
            (PropType::Number, ScriptValue::Number(n)) => {
                if !n.is_finite() {
                    return Err("a number property must be finite".to_owned());
                }
                let mut n = n;
                if let Some(min) = self.min {
                    n = n.max(min);
                }
                if let Some(max) = self.max {
                    n = n.min(max);
                }
                if !n.is_finite() {
                    return Err("a number property must stay finite after clamping".to_owned());
                }
                Ok(ScriptValue::Number(n))
            }
            (PropType::Bool, ScriptValue::Bool(b)) => Ok(ScriptValue::Bool(b)),
            (PropType::String, ScriptValue::String(s)) => Ok(ScriptValue::String(s)),
            (PropType::Asset, ScriptValue::String(s)) => Ok(ScriptValue::String(s)),
            (PropType::Vec3, ScriptValue::Vec3(v)) => {
                if !v.iter().all(|c| c.is_finite()) {
                    return Err("a vec3 property must be finite".to_owned());
                }
                Ok(ScriptValue::Vec3(v))
            }
            (PropType::Color, ScriptValue::List(items)) => {
                let mut channels = [1.0f64, 1.0, 1.0, 1.0];
                if !(3..=4).contains(&items.len()) {
                    return Err("a colour is a list of 3 or 4 numbers".to_owned());
                }
                for (index, item) in items.iter().enumerate() {
                    let ScriptValue::Number(n) = item else {
                        return Err("a colour is a list of numbers".to_owned());
                    };
                    if !n.is_finite() {
                        return Err("a colour channel must be finite".to_owned());
                    }
                    channels[index] = *n;
                }
                Ok(ScriptValue::List(
                    channels.iter().map(|c| ScriptValue::Number(*c)).collect(),
                ))
            }
            (PropType::Entity, ScriptValue::Entity(bits)) => Ok(ScriptValue::Entity(bits)),
            (PropType::Enum, ScriptValue::String(s)) if self.options.contains(&s) => {
                Ok(ScriptValue::String(s))
            }
            (PropType::Enum, ScriptValue::String(s)) => Err(format!(
                "`{s}` is not one of this property's options ({})",
                self.options.join(", ")
            )),
            (ty, value) => Err(format!(
                "a {} property takes {}, not {}",
                self.name,
                ty.name(),
                crate::value::describe(&value)
            )),
        }
    }
}

/// A component a mod declared with `regia.component`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentDecl {
    /// The component name; unique across every loaded mod.
    pub name: String,
    /// The declaring mod's id.
    pub mod_id: String,
    /// The declared properties, sorted by name (a Lua table keeps no declaration order).
    pub props: Vec<PropDecl>,
    /// Whether the declaration has an `on_update(entity, dt)` hook.
    pub has_update: bool,
}

impl ComponentDecl {
    /// The property `name`, when declared.
    #[must_use]
    pub fn prop(&self, name: &str) -> Option<&PropDecl> {
        self.props.iter().find(|p| p.name == name)
    }

    /// Every property's initial value, sorted by property name.
    #[must_use]
    pub fn initial_values(&self) -> Vec<(String, ScriptValue)> {
        self.props.iter().map(|p| (p.name.clone(), p.initial())).collect()
    }

    /// Merges stored `props` against this declaration: values of props still declared survive
    /// (re-coerced, so a type change falls back to the default), new props get their defaults,
    /// removed props go.
    #[must_use]
    pub fn reconcile(&self, props: &BTreeMap<String, LuaValue>) -> BTreeMap<String, LuaValue> {
        let mut out = BTreeMap::new();
        for prop in &self.props {
            let value = props.get(&prop.name).map_or_else(
                || LuaValue::from_script(&prop.initial()),
                |kept| match prop.coerce(kept.to_script()) {
                    Ok(coerced) => LuaValue::from_script(&coerced),
                    Err(_) => LuaValue::from_script(&prop.initial()),
                },
            );
            out.insert(prop.name.clone(), value);
        }
        out
    }
}

fn check_name(kind: &str, name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(format!(
            "a component {kind} is 1 to {MAX_NAME_BYTES} bytes, not `{}`",
            name.len()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(format!(
            "a component {kind} is lowercase ascii, digits and `_`: `{name}`"
        ));
    }
    Ok(())
}

/// A `lua_bad_value` error naming the property.
fn prop_error(name: &str, message: impl Into<String>) -> mlua::Error {
    crate::error::HostError::lua(
        crate::error::LuaErrorCode::BadValue,
        format!("property `{name}`: {}", message.into()),
    )
}

/// Parses one property declaration table (`{ type = ..., default = ..., ... }`).
fn parse_prop(name: &str, table: &Table) -> Result<PropDecl, mlua::Error> {
    let bad = |message: String| prop_error(name, message);
    let ty_text = table
        .raw_get::<Option<mlua::LuaString>>("type")
        .map_err(|e| bad(e.to_string()))?
        .map(|s| String::from_utf8_lossy(&s.as_bytes()).into_owned())
        .ok_or_else(|| bad(format!("property `{name}` has no `type`")))?;
    let ty = PropType::parse(&ty_text)
        .ok_or_else(|| bad(format!("property `{name}` has unknown type `{ty_text}`")))?;
    let number_opt = |key: &str| -> Result<Option<f64>, mlua::Error> {
        match table.raw_get::<Value>(key).map_err(|e| bad(e.to_string()))? {
            Value::Nil => Ok(None),
            Value::Integer(i) => Ok(Some(i as f64)),
            Value::Number(n) => Ok(Some(n)),
            other => Err(bad(format!(
                "property `{name}`'s `{key}` is a number, not {}",
                crate::value::lua_type(&other)
            ))),
        }
    };
    let min = number_opt("min")?;
    let max = number_opt("max")?;
    let step = number_opt("step")?;
    // Non-finite bounds are refused: a NaN bound would survive (`min > max` is false for NaN)
    // and be ignored by the clamp, and an infinite bound turns a finite value non-finite.
    for (key, bound) in [("min", min), ("max", max), ("step", step)] {
        if let Some(bound) = bound
            && !bound.is_finite()
        {
            return Err(bad(format!("property `{name}`'s `{key}` is finite")));
        }
    }
    if let (Some(min), Some(max)) = (min, max)
        && min > max
    {
        return Err(bad(format!("property `{name}` has min above max")));
    }
    let mut options = Vec::new();
    if ty == PropType::Enum {
        let list: Table = table
            .raw_get("options")
            .map_err(|_| bad("an enum property needs `options`, a list of strings".to_string()))?;
        for (walked, item) in list.sequence_values::<mlua::LuaString>().enumerate() {
            // The whole list counts, repeats included: a list of one string repeated a million
            // times held one option and cost a million host-side copies and compares.
            if walked >= MAX_ENUM_OPTIONS {
                return Err(bad(format!(
                    "an enum property has at most {MAX_ENUM_OPTIONS} options"
                )));
            }
            let item = item.map_err(|e| bad(e.to_string()))?;
            let bytes = item.as_bytes();
            if bytes.len() > MAX_ENUM_OPTION_BYTES {
                return Err(bad(format!(
                    "an enum option is at most {MAX_ENUM_OPTION_BYTES} bytes, not {}",
                    bytes.len()
                )));
            }
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if !options.contains(&text) {
                options.push(text);
            }
        }
        if options.is_empty() {
            return Err(bad(format!("property `{name}`'s `options` is empty")));
        }
    }
    let filter = match table.raw_get::<Value>("filter").map_err(|e| bad(e.to_string()))? {
        Value::Nil => None,
        Value::String(s) if s.as_bytes().len() <= 256 => {
            Some(String::from_utf8_lossy(&s.as_bytes()).into_owned())
        }
        _ => {
            return Err(bad(format!(
                "property `{name}`'s `filter` is a string of at most 256 bytes"
            )));
        }
    };
    let multiline = matches!(
        table
            .raw_get::<Value>("multiline")
            .map_err(|e| bad(e.to_string()))?,
        Value::Boolean(true)
    );
    let default = match table
        .raw_get::<Value>("default")
        .map_err(|e| bad(e.to_string()))?
    {
        Value::Nil => default_of(ty, &options),
        value => ScriptValue::from_lua(&value).map_err(|e| bad(e.to_string()))?,
    };
    let decl = PropDecl {
        name: name.to_owned(),
        ty,
        default: ScriptValue::Nil,
        min,
        max,
        step,
        options,
        filter,
        multiline,
    };
    let default = decl.coerce(default).map_err(bad)?;
    Ok(PropDecl { default, ..decl })
}

/// The value a property starts from when the declaration sets no `default`.
fn default_of(ty: PropType, options: &[String]) -> ScriptValue {
    match ty {
        PropType::Number => ScriptValue::Number(0.0),
        PropType::Bool => ScriptValue::Bool(false),
        PropType::String | PropType::Asset => ScriptValue::String(String::new()),
        PropType::Vec3 => ScriptValue::Vec3([0.0; 3]),
        PropType::Color => ScriptValue::List(vec![
            ScriptValue::Number(1.0),
            ScriptValue::Number(1.0),
            ScriptValue::Number(1.0),
            ScriptValue::Number(1.0),
        ]),
        PropType::Entity => ScriptValue::Nil,
        PropType::Enum => options
            .first()
            .map_or(ScriptValue::Nil, |o| ScriptValue::String(o.clone())),
    }
}

/// Parses a `regia.component(name, decl_table)` call into a [`ComponentDecl`].
///
/// # Errors
/// Why the declaration is invalid (unknown type, bad default, over the caps), as a Lua error.
pub(crate) fn parse_decl(mod_id: &str, name: &str, table: &Table) -> mlua::Result<ComponentDecl> {
    let bad = |message: String| {
        crate::error::HostError::lua(
            crate::error::LuaErrorCode::BadValue,
            format!("component `{name}`: {message}"),
        )
    };
    check_name("name", name).map_err(bad)?;
    let Value::Table(props_table) = table
        .raw_get("props")
        .map_err(|e| bad(format!("`{name}`: {e}")))?
    else {
        return Err(bad(format!("`{name}` needs a `props` table")));
    };
    let mut props = Vec::new();
    let mut weight = 0_u64;
    for pair in props_table.pairs::<mlua::LuaString, Table>() {
        let (key, prop_table) = pair.map_err(|e| {
            bad(format!(
                "`{name}`'s `props` maps names to declaration tables: {e}"
            ))
        })?;
        let prop_name = String::from_utf8_lossy(&key.as_bytes()).into_owned();
        if props.len() >= MAX_PROPS_PER_DECL {
            return Err(bad(format!(
                "`{name}` has more than {MAX_PROPS_PER_DECL} properties"
            )));
        }
        check_name("property name", &prop_name).map_err(|e| bad(format!("`{name}`: {e}")))?;
        if props.iter().any(|p: &PropDecl| p.name == prop_name) {
            return Err(bad(format!("`{name}` declares `{prop_name}` twice")));
        }
        let prop = parse_prop(&prop_name, &prop_table)?;
        weight = weight.saturating_add(prop.weight());
        if weight > MAX_DECL_BYTES {
            return Err(bad(format!(
                "`{name}` holds more than {MAX_DECL_BYTES} bytes of names, options and defaults"
            )));
        }
        props.push(prop);
    }
    // A Lua table keeps no declaration order (`pairs` walks the hash part, and adding one
    // property reshuffles the rest), so the declaration is sorted by name: the inspector, the
    // agent schema and `initial_values` list the same order on every load (ADR-5517).
    if props.is_empty() {
        return Err(bad(format!("`{name}` declares no properties")));
    }
    props.sort_by(|a, b| a.name.cmp(&b.name));
    let has_update = !matches!(
        table
            .raw_get::<Value>("on_update")
            .map_err(|e| bad(e.to_string()))?,
        Value::Nil
    );
    Ok(ComponentDecl {
        name: name.to_owned(),
        mod_id: mod_id.to_owned(),
        props,
        has_update,
    })
}

/// A per-entity property value as stored on the ECS (the reflected form of [`ScriptValue`]):
/// scene files persist it through the reflection codec.
#[derive(Reflect, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LuaValue {
    /// No value (an `entity` property with no reference).
    Nil,
    /// A boolean.
    Bool(bool),
    /// A number.
    Number(f64),
    /// A string.
    String(String),
    /// A vector.
    Vec3([f32; 3]),
    /// An entity's bits.
    Entity(u64),
    /// A list (a `color` is a list of four numbers).
    List(Vec<LuaValue>),
    /// A map with string keys.
    Map(std::collections::BTreeMap<String, LuaValue>),
}

impl LuaValue {
    /// The mirror of `value`.
    #[must_use]
    pub fn from_script(value: &ScriptValue) -> Self {
        match value {
            ScriptValue::Nil => Self::Nil,
            ScriptValue::Bool(b) => Self::Bool(*b),
            ScriptValue::Number(n) => Self::Number(*n),
            ScriptValue::String(s) => Self::String(s.clone()),
            ScriptValue::Vec3(v) => Self::Vec3(*v),
            ScriptValue::Entity(bits) => Self::Entity(*bits),
            ScriptValue::List(items) => Self::List(items.iter().map(Self::from_script).collect()),
            ScriptValue::Map(map) => Self::Map(
                map.iter()
                    .map(|(k, v)| (k.clone(), Self::from_script(v)))
                    .collect(),
            ),
        }
    }

    /// The plain value crossing the Lua and agent boundaries.
    #[must_use]
    pub fn to_script(&self) -> ScriptValue {
        match self {
            Self::Nil => ScriptValue::Nil,
            Self::Bool(b) => ScriptValue::Bool(*b),
            Self::Number(n) => ScriptValue::Number(*n),
            Self::String(s) => ScriptValue::String(s.clone()),
            Self::Vec3(v) => ScriptValue::Vec3(*v),
            Self::Entity(bits) => ScriptValue::Entity(*bits),
            Self::List(items) => ScriptValue::List(items.iter().map(Self::to_script).collect()),
            Self::Map(map) => ScriptValue::Map(map.iter().map(|(k, v)| (k.clone(), v.to_script())).collect()),
        }
    }
}

/// Per-entity values of one declared script component ([`ComponentDecl`]).
///
/// Attached by a mod's `e:add("name")`, edited per entity in the inspector, saved in the scene
/// like any reflected component. `props` holds exactly the declared properties (reconciled
/// against the declaration on load, hot reload and restart).
#[derive(Component, Reflect, Clone, Default, Debug, PartialEq)]
#[reflect(Component, Default, Clone)]
pub struct LuaComponent {
    /// The declared component name.
    pub decl: String,
    /// The entity's values, keyed by property name.
    pub props: BTreeMap<String, LuaValue>,
}

impl LuaComponent {
    /// A fresh attachment of `decl` with every property at its default.
    #[must_use]
    pub fn from_decl(decl: &ComponentDecl) -> Self {
        Self {
            decl: decl.name.clone(),
            props: decl
                .initial_values()
                .into_iter()
                .map(|(name, value)| (name, LuaValue::from_script(&value)))
                .collect(),
        }
    }

    /// The value of property `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&LuaValue> {
        self.props.get(name)
    }

    /// Replaces the values with `values` (already coerced against the declaration).
    pub fn set_values(&mut self, values: Vec<(String, ScriptValue)>) {
        self.props = values
            .into_iter()
            .map(|(name, value)| (name, LuaValue::from_script(&value)))
            .collect();
    }

    /// The values as plain data, sorted by property name.
    #[must_use]
    pub fn values(&self) -> Vec<(String, ScriptValue)> {
        self.props
            .iter()
            .map(|(name, value)| (name.clone(), value.to_script()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Result<ComponentDecl, String> {
        let lua = mlua::Lua::new();
        // The `regia.vec3` constructor the declaration's defaults may call.
        let regia = lua.create_table().expect("table");
        regia
            .raw_set(
                "vec3",
                lua.create_function(|_, (x, y, z): (Option<f32>, Option<f32>, Option<f32>)| {
                    Ok(crate::value::LuaVec3(regia_core::math::Vec3::new(
                        x.unwrap_or(0.0),
                        y.unwrap_or(0.0),
                        z.unwrap_or(0.0),
                    )))
                })
                .expect("fn"),
            )
            .expect("set");
        lua.globals().raw_set("regia", regia).expect("regia");
        let table: Table = lua.load(src).eval().map_err(|e| e.to_string())?;
        parse_decl("demo", "mover", &table).map_err(|e| e.to_string())
    }

    #[test]
    fn a_full_declaration_parses_with_defaults_and_ranges() {
        let decl = parse(
            r#"{
                props = {
                    speed  = { type = "number", default = 2.0, min = 0.0, max = 20.0, step = 0.1 },
                    active = { type = "bool", default = true },
                    label  = { type = "string", default = "mover" },
                    offset = { type = "vec3", default = regia.vec3(0, 1, 0) },
                    tint   = { type = "color", default = { 1.0, 0.5, 0.25 } },
                    target = { type = "entity" },
                    mesh   = { type = "asset", filter = "mesh" },
                    mode   = { type = "enum", options = { "walk", "run" }, default = "run" },
                },
                on_update = function(e, dt) end,
            }"#,
        )
        .expect("declaration parses");
        assert_eq!(decl.name, "mover");
        assert!(decl.has_update);
        assert_eq!(decl.props.len(), 8);
        let speed = decl.prop("speed").expect("speed");
        assert_eq!(speed.ty, PropType::Number);
        assert_eq!(speed.default, ScriptValue::Number(2.0));
        assert_eq!(speed.min, Some(0.0));
        assert_eq!(speed.max, Some(20.0));
        assert_eq!(speed.step, Some(0.1));
        assert_eq!(
            decl.prop("mode").expect("mode").default,
            ScriptValue::String("run".into())
        );
        assert_eq!(
            decl.prop("tint").expect("tint").default,
            ScriptValue::List(vec![
                ScriptValue::Number(1.0),
                ScriptValue::Number(0.5),
                ScriptValue::Number(0.25),
                ScriptValue::Number(1.0),
            ])
        );
        assert_eq!(
            decl.prop("offset").expect("offset").default,
            ScriptValue::Vec3([0.0, 1.0, 0.0])
        );
    }

    #[test]
    fn missing_defaults_resolve_to_type_zeros() {
        let decl = parse(
            r#"{
                props = {
                    n = { type = "number" },
                    b = { type = "bool" },
                    s = { type = "string" },
                    v = { type = "vec3" },
                    c = { type = "color" },
                    e = { type = "entity" },
                    m = { type = "asset" },
                    g = { type = "enum", options = { "a", "b" } },
                },
            }"#,
        )
        .expect("declaration parses");
        assert!(!decl.has_update);
        let values: BTreeMap<String, ScriptValue> = decl.initial_values().into_iter().collect();
        assert_eq!(values["n"], ScriptValue::Number(0.0));
        assert_eq!(values["b"], ScriptValue::Bool(false));
        assert_eq!(values["s"], ScriptValue::String(String::new()));
        assert_eq!(values["v"], ScriptValue::Vec3([0.0; 3]));
        assert_eq!(values["e"], ScriptValue::Nil);
        assert_eq!(values["g"], ScriptValue::String("a".into()));
    }

    #[test]
    fn bad_declarations_are_refused() {
        for src in [
            r#"{ props = { speed = { type = "nunber" } } }"#,
            r#"{ props = { speed = { type = "number", min = 10, max = 2 } } }"#,
            r#"{ props = { speed = { type = "number", default = "fast" } } }"#,
            r#"{ props = { mode = { type = "enum", options = {} } } }"#,
            r#"{ props = { mode = { type = "enum", options = { "a" }, default = "b" } } }"#,
            r#"{ props = { tint = { type = "color", default = { 1, "x", 3 } } } }"#,
            r#"{ props = { ["Bad Name"] = { type = "bool" } } }"#,
            r#"{ props = {} }"#,
            r#"{ }"#,
            r#"{ props = { speed = { type = "number", default = 0/0 } } }"#,
        ] {
            assert!(parse(src).is_err(), "{src}");
        }
    }

    /// Sec review (ADR-5870 § 1): the `options` walk ran on the host, unbudgeted, over the whole
    /// list; only distinct options counted toward the cap, so one string repeated a million times
    /// cost a million copies and compares and was accepted.
    #[test]
    fn an_options_list_longer_than_the_cap_is_refused_even_when_its_items_repeat() {
        let at_cap = format!(
            r#"local o = {{}} for i = 1, {MAX_ENUM_OPTIONS} do o[i] = "a" end
               return {{ props = {{ mode = {{ type = "enum", options = o }} }} }}"#
        );
        let decl = parse(&at_cap).expect("a list at the cap parses");
        assert_eq!(decl.prop("mode").expect("mode").options, ["a"]);
        let over = format!(
            r#"local o = {{}} for i = 1, {} do o[i] = "a" end
               return {{ props = {{ mode = {{ type = "enum", options = o }} }} }}"#,
            MAX_ENUM_OPTIONS + 1
        );
        let error = parse(&over).expect_err("one item past the cap is refused");
        assert!(error.contains("at most"), "{error}");
    }

    /// Sec review (ADR-5870 § 1): option texts had no length cap, so 64 props of 64 options of
    /// 200 KB each kept 820 MB per declaration on the Rust heap for a few hundred instructions.
    #[test]
    fn an_option_longer_than_the_cap_is_refused() {
        let ok = format!(
            r#"return {{ props = {{ mode = {{ type = "enum", options = {{ string.rep("x", {MAX_ENUM_OPTION_BYTES}) }} }} }} }}"#
        );
        assert!(parse(&ok).is_ok());
        let long = format!(
            r#"return {{ props = {{ mode = {{ type = "enum", options = {{ string.rep("x", {}) }} }} }} }}"#,
            MAX_ENUM_OPTION_BYTES + 1
        );
        assert!(parse(&long).is_err());
    }

    /// Sec review (ADR-5870 § 1): a declaration's defaults and options together are bounded, so
    /// 64 declarations of 64 string defaults of 1 MiB (4 GiB) cannot be kept and copied.
    #[test]
    fn a_declaration_heavier_than_its_cap_is_refused() {
        let ok = r#"return { props = { label = { type = "string", default = string.rep("x", 1000) } } }"#;
        assert!(parse(ok).is_ok());
        let heavy = format!(
            r#"return {{ props = {{ label = {{ type = "string", default = string.rep("x", {MAX_DECL_BYTES}) }} }} }}"#
        );
        let error = parse(&heavy).expect_err("a default over the declaration cap is refused");
        assert!(error.contains("bytes"), "{error}");
        // Spread over many properties, the total still counts.
        let spread = r#"local p = {}
            for i = 1, 40 do p["s" .. i] = { type = "string", default = string.rep("x", 2000) } end
            return { props = p }"#;
        assert!(parse(spread).is_err());
    }

    /// Spec 84 § 14 limits at their edges: 64 properties and 64 enum options are accepted and
    /// one more is refused; property and component names are 1-64 bytes of lowercase ascii,
    /// digits and `_`.
    #[test]
    fn declaration_limits_hold_at_their_edges() {
        let props = |n: usize| {
            let body: Vec<String> = (0..n).map(|i| format!("p{i} = {{ type = \"bool\" }}")).collect();
            format!("{{ props = {{ {} }} }}", body.join(", "))
        };
        assert_eq!(
            parse(&props(MAX_PROPS_PER_DECL))
                .expect("64 properties")
                .props
                .len(),
            64
        );
        assert!(parse(&props(MAX_PROPS_PER_DECL + 1)).is_err());
        let options = |n: usize| {
            let list: Vec<String> = (0..n).map(|i| format!("\"o{i}\"")).collect();
            format!(
                "{{ props = {{ mode = {{ type = \"enum\", options = {{ {} }} }} }} }}",
                list.join(", ")
            )
        };
        assert!(parse(&options(MAX_ENUM_OPTIONS)).is_ok());
        assert!(parse(&options(MAX_ENUM_OPTIONS + 1)).is_err());
        let named = |name: &str| format!("{{ props = {{ [\"{name}\"] = {{ type = \"bool\" }} }} }}");
        let longest = "a".repeat(MAX_NAME_BYTES);
        assert!(parse(&named(&longest)).is_ok());
        for bad in [
            String::new(),
            "a".repeat(MAX_NAME_BYTES + 1),
            "Speed".to_owned(),
            "top-speed".to_owned(),
            "top speed".to_owned(),
        ] {
            assert!(parse(&named(&bad)).is_err(), "{bad:?}");
        }
        assert!(parse(&named("speed_2")).is_ok());
        // The component name follows the same rule.
        let lua = mlua::Lua::new();
        let table: Table = lua.load(props(1)).eval().expect("a table");
        assert!(parse_decl("demo", &longest, &table).is_ok());
        for bad in ["", "Mover", "mo-ver"] {
            assert!(parse_decl("demo", bad, &table).is_err(), "{bad:?}");
        }
        assert!(parse_decl("demo", &"m".repeat(MAX_NAME_BYTES + 1), &table).is_err());
    }

    #[test]
    fn coerce_checks_types_and_clamps_numbers() {
        let decl = parse(
            r#"{ props = { speed = { type = "number", min = 0, max = 10 }, mode = { type = "enum", options = { "a", "b" } } } }"#,
        )
        .expect("declaration parses");
        let speed = decl.prop("speed").expect("speed");
        assert_eq!(
            speed.coerce(ScriptValue::Number(30.0)).expect("clamped"),
            ScriptValue::Number(10.0)
        );
        assert_eq!(
            speed.coerce(ScriptValue::Number(-3.0)).expect("clamped"),
            ScriptValue::Number(0.0)
        );
        assert!(speed.coerce(ScriptValue::Bool(true)).is_err());
        let mode = decl.prop("mode").expect("mode");
        assert!(mode.coerce(ScriptValue::String("z".into())).is_err());
    }

    #[test]
    fn coerce_rejects_non_finite_numbers_and_vec3s() {
        let decl = parse(
            r#"{ props = { speed = { type = "number", min = 0, max = 10 }, offset = { type = "vec3" } } }"#,
        )
        .expect("declaration parses");
        let speed = decl.prop("speed").expect("speed");
        assert!(speed.coerce(ScriptValue::Number(f64::NAN)).is_err());
        assert!(speed.coerce(ScriptValue::Number(f64::INFINITY)).is_err());
        let offset = decl.prop("offset").expect("offset");
        // A NaN or infinite vec3 is refused like a non-finite number: it would silently freeze
        // the entity's moves (set_position rejects it) and persist into the scene file.
        assert!(offset.coerce(ScriptValue::Vec3([f32::NAN, 1.0, 0.0])).is_err());
        assert!(
            offset
                .coerce(ScriptValue::Vec3([0.0, f32::INFINITY, 0.0]))
                .is_err()
        );
        assert!(
            offset
                .coerce(ScriptValue::Vec3([0.0, 0.0, f32::NEG_INFINITY]))
                .is_err()
        );
        assert_eq!(
            offset.coerce(ScriptValue::Vec3([1.0, 2.0, 3.0])).expect("finite"),
            ScriptValue::Vec3([1.0, 2.0, 3.0])
        );
    }

    #[test]
    fn non_finite_bounds_are_refused_and_cannot_leak_through_clamping() {
        // Parsing refuses non-finite min/max/step: a NaN bound would be kept (NaN comparisons
        // are false) and ignored by the clamp, and an infinite bound turns a finite default or
        // value non-finite.
        for src in [
            r#"{ props = { speed = { type = "number", min = 1/0 } } }"#,
            r#"{ props = { speed = { type = "number", max = -1/0 } } }"#,
            r#"{ props = { speed = { type = "number", min = 0/0 } } }"#,
            r#"{ props = { speed = { type = "number", max = 0/0 } } }"#,
            r#"{ props = { speed = { type = "number", step = 1/0 } } }"#,
            // A non-finite vec3 default is refused too.
            r#"{ props = { offset = { type = "vec3", default = regia.vec3(0/0, 1, 0) } } }"#,
        ] {
            assert!(parse(src).is_err(), "{src}");
        }
        // Even a declaration built by hand (or read from an old scene) cannot clamp a finite
        // value into a non-finite one: the finite check runs again after the clamp.
        let mut decl = parse(r#"{ props = { speed = { type = "number" } } }"#).expect("parses");
        decl.props[0].min = Some(f64::INFINITY);
        assert!(decl.props[0].coerce(ScriptValue::Number(0.0)).is_err());
        decl.props[0].min = None;
        decl.props[0].max = Some(f64::NEG_INFINITY);
        assert!(decl.props[0].coerce(ScriptValue::Number(0.0)).is_err());
    }

    /// Properties come out sorted by name, whatever order the table constructor wrote them in.
    /// Before the fix they came out in the table's hash order (for this declaration `offset`
    /// first), which the doc called declaration order.
    #[test]
    fn props_are_listed_by_name() {
        let names = |src: &str| -> Vec<String> {
            parse(src)
                .expect("declaration parses")
                .props
                .into_iter()
                .map(|p| p.name)
                .collect()
        };
        let sorted = ["mode", "offset", "speed", "target", "tint"];
        let written = r#"{ props = {
            speed  = { type = "number", default = 2.0, min = 0.0, max = 20.0 },
            mode   = { type = "enum", options = { "walk", "run" } },
            offset = { type = "vec3", default = regia.vec3(0, 1, 0) },
            target = { type = "entity" },
            tint   = { type = "color", default = { 1.0, 0.5, 0.25, 1.0 } },
        } }"#;
        assert_eq!(names(written), sorted);
        let reversed = r#"{ props = {
            tint   = { type = "color" },
            target = { type = "entity" },
            offset = { type = "vec3" },
            mode   = { type = "enum", options = { "walk", "run" } },
            speed  = { type = "number" },
        } }"#;
        assert_eq!(names(reversed), sorted);
    }

    /// ADR-5517: the properties come back sorted by name whatever order the source lists them
    /// in (a Lua table keeps none), so the schema, `initial_values` and the inspector agree on
    /// every load.
    #[test]
    fn props_list_sorted_by_name() {
        let decl = parse(
            r#"{ props = {
                zeta = { type = "bool" }, alpha = { type = "number" }, mid = { type = "string" },
                beta = { type = "vec3" }, omega = { type = "entity" },
            } }"#,
        )
        .expect("parses");
        let names: Vec<&str> = decl.props.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["alpha", "beta", "mid", "omega", "zeta"]);
        let initial: Vec<String> = decl.initial_values().into_iter().map(|(n, _)| n).collect();
        assert_eq!(initial, ["alpha", "beta", "mid", "omega", "zeta"]);
        // Adding a property keeps the others in the same relative order.
        let more = parse(
            r#"{ props = {
                zeta = { type = "bool" }, alpha = { type = "number" }, mid = { type = "string" },
                beta = { type = "vec3" }, omega = { type = "entity" }, kappa = { type = "bool" },
            } }"#,
        )
        .expect("parses");
        let names: Vec<&str> = more.props.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["alpha", "beta", "kappa", "mid", "omega", "zeta"]);
    }

    #[test]
    fn reconcile_keeps_edits_defaults_new_and_drops_removed() {
        let mut decl = parse(
            r#"{ props = { speed = { type = "number", default = 2 }, label = { type = "string", default = "x" } } }"#,
        )
        .expect("declaration parses");
        let mut stored = BTreeMap::new();
        stored.insert("speed".to_owned(), LuaValue::Number(7.0));
        stored.insert("gone".to_owned(), LuaValue::Number(1.0));
        // The reload renames `label`, changes `speed`'s default and adds `fresh`.
        decl.props.retain(|p| p.name != "label");
        decl.props.push(PropDecl {
            name: "fresh".to_owned(),
            ty: PropType::Bool,
            default: ScriptValue::Bool(true),
            min: None,
            max: None,
            step: None,
            options: Vec::new(),
            filter: None,
            multiline: false,
        });
        let merged = decl.reconcile(&stored);
        assert_eq!(
            merged["speed"],
            LuaValue::Number(7.0),
            "the edited value survives"
        );
        assert_eq!(
            merged["fresh"],
            LuaValue::Bool(true),
            "a new prop gets its default"
        );
        assert!(!merged.contains_key("gone"), "a removed prop goes");
        assert!(!merged.contains_key("label"), "a renamed prop goes");
    }

    #[test]
    fn a_type_change_falls_back_to_the_default() {
        let decl =
            parse(r#"{ props = { speed = { type = "number", default = 2 } } }"#).expect("declaration parses");
        let mut stored = BTreeMap::new();
        stored.insert("speed".to_owned(), LuaValue::String("fast".into()));
        let merged = decl.reconcile(&stored);
        assert_eq!(merged["speed"], LuaValue::Number(2.0));
    }

    #[test]
    fn lua_component_round_trips_through_script_values() {
        let decl = parse(
            r#"{ props = { speed = { type = "number", default = 2 }, target = { type = "entity" } } }"#,
        )
        .expect("declaration parses");
        let mut component = LuaComponent::from_decl(&decl);
        component.set_values(vec![
            ("speed".to_owned(), ScriptValue::Number(5.0)),
            ("target".to_owned(), ScriptValue::Entity(9)),
        ]);
        assert_eq!(component.get("speed"), Some(&LuaValue::Number(5.0)));
        let values: BTreeMap<String, ScriptValue> = component.values().into_iter().collect();
        assert_eq!(values["speed"], ScriptValue::Number(5.0));
        assert_eq!(values["target"], ScriptValue::Entity(9));
    }
}
