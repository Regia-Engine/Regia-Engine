// SPDX-License-Identifier: MPL-2.0
//! A utility panel's driver (ADR-8189): the logic graph or Lua script that turns one UI command
//! into a list of editor calls. A driver never touches the editor world: it gets a
//! [`PanelContext`] (the command, the panel's named values and the selection) and answers
//! [`PanelCall`]s, which the host checks against the safe subset and runs.
//!
//! - **Logic graph** (`.logic.ron`): the command is an inbox event (`EntityEvent("<command>")`);
//!   the panel's values are in the entity blackboard (key = node name), and `selection_count` holds
//!   the number of selected entities. The action `editor::Call` asks for one call: `method` names
//!   it, every other `set` entry is a parameter. With `for_each_selected: Bool(true)` it is one call
//!   per selected entity with `entity` set to it. In string parameters `{name}` is the entity's
//!   name and `{<value>}` a panel value (`"{prefix}{name}"`); `{{` and `}}` are literal braces.
//! - **Lua** (`.lua`, the mod sandbox of `docs/specs/84-lua-mods.md`): `regia.on("command",
//!   function(ctx) ... end)` gets `ctx.command`, `ctx.values` and `ctx.selection` (a list of
//!   `{ entity, name }`, `entity` the entity's bits as a number); `regia.emit("editor.call", { method
//!   = ..., params = { ... } })` asks for a call (whole numbers go out as integers).

use std::collections::BTreeMap;

use regia_ai::logic::{LogicGraph, LogicGraphDef};
use regia_ai::{BbValue, Blackboard, BtContext, BtStatus, BtTask, BtTaskRegistry};
use regia_ecs::prelude::*;
use regia_script_lua::{LuaLimits, LuaMods, ModFiles, ModStatus, NoWorld, ScriptValue};
use serde_json::{Map, Value, json};

/// The task path of the logic graph's editor call action.
pub const LOGIC_CALL_TASK: &str = "editor::Call";
/// The Lua event a command arrives as.
pub const LUA_COMMAND_EVENT: &str = "command";
/// The Lua event a call request is emitted as.
pub const LUA_CALL_EVENT: &str = "editor.call";
/// The most calls one command may ask for.
pub const MAX_CALLS_PER_COMMAND: usize = 256;

/// What a driver sees of one command.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PanelContext {
    /// The command name from the layout (`command: "rename"`).
    pub command: String,
    /// The panel's named nodes' current text (text inputs, toggles as `On`/`Off`), by node name.
    pub values: BTreeMap<String, String>,
    /// The selected entities (bits) and their level names, in selection order.
    pub selection: Vec<(u64, String)>,
}

/// One editor call a driver asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelCall {
    /// The method (`editor.outliner_rename`).
    pub method: String,
    /// Its parameters (a JSON object).
    pub params: Value,
}

/// Why a driver failed. `code` is the agent error code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverError {
    /// `utility_panel_driver_load` or `utility_panel_driver_failed`.
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

impl DriverError {
    fn load(message: impl Into<String>) -> Self {
        Self {
            code: "utility_panel_driver_load",
            message: message.into(),
        }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self {
            code: "utility_panel_driver_failed",
            message: message.into(),
        }
    }
}

/// A loaded driver.
pub enum PanelDriver {
    /// A compiled logic graph in its own scratch world.
    Logic(Box<LogicDriver>),
    /// One sandboxed Lua mod.
    Lua(Box<LuaDriver>),
}

impl PanelDriver {
    /// Loads a logic graph driver from `.logic.ron` text.
    ///
    /// # Errors
    /// `utility_panel_driver_load` with the graph's parse or compile error.
    pub fn logic(text: &str) -> Result<Self, DriverError> {
        LogicDriver::new(text).map(|d| Self::Logic(Box::new(d)))
    }

    /// Loads a Lua driver from script text; `title` names the mod in its log.
    ///
    /// # Errors
    /// `utility_panel_driver_load` with the sandbox's load error.
    pub fn lua(title: &str, script: &str) -> Result<Self, DriverError> {
        LuaDriver::new(title, script).map(|d| Self::Lua(Box::new(d)))
    }

    /// Runs one command and returns the calls it asked for, in order.
    ///
    /// # Errors
    /// `utility_panel_driver_failed`: the graph's action or the script failed (the message says
    /// why), or the command asked for more than [`MAX_CALLS_PER_COMMAND`] calls.
    pub fn run(&mut self, ctx: &PanelContext) -> Result<Vec<PanelCall>, DriverError> {
        let _span =
            tracing::info_span!(target: "regia_editor", "utility_panel_driver", command = %ctx.command)
                .entered();
        let calls = match self {
            Self::Logic(d) => d.run(ctx)?,
            Self::Lua(d) => d.run(ctx)?,
        };
        if calls.len() > MAX_CALLS_PER_COMMAND {
            return Err(DriverError::failed(format!(
                "the command asked for {} calls (at most {MAX_CALLS_PER_COMMAND})",
                calls.len()
            )));
        }
        Ok(calls)
    }
}

// ---------------------------------------------------------------------------------------------
// Logic graph

/// The calls the graph's `editor::Call` actions asked for during one tick (scratch world only).
#[derive(Resource, Default)]
struct CallSink {
    calls: Vec<RawCall>,
}

/// A call before its selection expansion and templates.
struct RawCall {
    method: String,
    each: bool,
    params: Map<String, Value>,
}

/// The `editor::Call` action: records its parameters as one call request.
#[derive(Default)]
struct EditorCallTask {
    params: Vec<(String, BbValue)>,
}

fn bb_json(value: &BbValue) -> Value {
    match value {
        BbValue::Bool(b) => json!(b),
        BbValue::Int(i) => json!(i),
        BbValue::Float(f) => json!(f),
        BbValue::String(s) => json!(s),
        BbValue::Vec3(v) => json!([v.x, v.y, v.z]),
        BbValue::Entity(bits) => json!(bits),
    }
}

impl BtTask for EditorCallTask {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let mut method = None;
        let mut each = false;
        let mut params = Map::new();
        for (key, value) in &self.params {
            match (key.as_str(), value) {
                ("method", BbValue::String(name)) => method = Some(name.clone()),
                ("for_each_selected", BbValue::Bool(flag)) => each = *flag,
                ("method" | "for_each_selected", _) => return BtStatus::Failure,
                _ => {
                    params.insert(key.clone(), bb_json(value));
                }
            }
        }
        let Some(method) = method else {
            return BtStatus::Failure;
        };
        ctx.world
            .get_resource_or_insert_with(CallSink::default)
            .calls
            .push(RawCall { method, each, params });
        BtStatus::Success
    }

    fn logic_params(&mut self, params: &[(String, BbValue)]) {
        self.params.clear();
        self.params.extend_from_slice(params);
    }
}

/// A logic graph driver: the graph runs on one entity of a private world.
pub struct LogicDriver {
    world: World,
    entity: Entity,
    graph: LogicGraph,
}

impl LogicDriver {
    fn new(text: &str) -> Result<Self, DriverError> {
        let def =
            LogicGraphDef::from_ron(text).map_err(|e| DriverError::load(format!("logic graph: {e}")))?;
        let mut registry = BtTaskRegistry::default();
        registry.register_factory(LOGIC_CALL_TASK, || Box::new(EditorCallTask::default()));
        let graph = LogicGraph::compile(&def, &registry)
            .map_err(|e| DriverError::load(format!("logic graph: {e} ({})", e.reason())))?;
        let mut world = World::new();
        world.init_resource::<CallSink>();
        let entity = world.spawn(Blackboard::new()).id();
        let mut driver = Self { world, entity, graph };
        // The start tick: `Start` rules run and the blackboard baseline is taken.
        driver.tick();
        driver.world.resource_mut::<CallSink>().calls.clear();
        Ok(driver)
    }

    fn tick(&mut self) {
        let mut ctx = BtContext {
            world: &mut self.world,
            entity: self.entity,
            dt: 0.0,
        };
        self.graph.tick(&mut ctx, 1);
    }

    fn run(&mut self, ctx: &PanelContext) -> Result<Vec<PanelCall>, DriverError> {
        if let Some(mut board) = self.world.get_mut::<Blackboard>(self.entity) {
            for (key, value) in &ctx.values {
                if !key.is_empty() && !key.contains('/') {
                    board.insert(key.clone(), BbValue::String(value.clone()));
                }
            }
            board.insert(
                "selection_count",
                BbValue::Int(i64::try_from(ctx.selection.len()).unwrap_or(i64::MAX)),
            );
        }
        let total_before = self.graph.trace_total();
        self.graph.send_event(ctx.command.clone());
        self.tick();
        let raw = std::mem::take(&mut self.world.resource_mut::<CallSink>().calls);
        // This tick's trace lines (the ring keeps the newest): a failed action fails the command.
        let new =
            usize::try_from(self.graph.trace_total().saturating_sub(total_before)).unwrap_or(usize::MAX);
        let len = self.graph.trace_len();
        let mut line = String::new();
        for i in len.saturating_sub(new)..len {
            line.clear();
            self.graph.write_trace_line(i, &mut line);
            if line.contains("action_failed") {
                return Err(DriverError::failed(format!("logic graph: {line}")));
            }
        }
        let mut calls = Vec::new();
        for call in raw {
            if call.each {
                for (entity, name) in &ctx.selection {
                    let mut params = Map::new();
                    for (key, value) in &call.params {
                        params.insert(key.clone(), template(value, ctx, Some(name))?);
                    }
                    params.insert("entity".into(), json!(entity));
                    calls.push(PanelCall {
                        method: call.method.clone(),
                        params: Value::Object(params),
                    });
                }
            } else {
                let mut params = Map::new();
                for (key, value) in &call.params {
                    params.insert(key.clone(), template(value, ctx, None)?);
                }
                calls.push(PanelCall {
                    method: call.method,
                    params: Value::Object(params),
                });
            }
            if calls.len() > MAX_CALLS_PER_COMMAND {
                break;
            }
        }
        Ok(calls)
    }
}

/// Expands `{name}` and `{<panel value>}` in a string parameter (`{{`/`}}` are literal braces);
/// other values pass through.
fn template(value: &Value, ctx: &PanelContext, name: Option<&str>) -> Result<Value, DriverError> {
    let Value::String(text) = value else {
        return Ok(value.clone());
    };
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut key = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(c) => key.push(c),
                        None => {
                            return Err(DriverError::failed(format!("unclosed `{{` in {text:?}")));
                        }
                    }
                }
                match (key.as_str(), name) {
                    ("name", Some(name)) => out.push_str(name),
                    _ => match ctx.values.get(&key) {
                        Some(value) => out.push_str(value),
                        None => {
                            return Err(DriverError::failed(format!(
                                "{{{key}}} in {text:?} is not a panel value"
                            )));
                        }
                    },
                }
            }
            c => out.push(c),
        }
    }
    Ok(Value::String(out))
}

// ---------------------------------------------------------------------------------------------
// Lua

/// The id of the panel's mod inside its own `LuaMods`.
const LUA_MOD_ID: &str = "utility_panel";

/// A Lua driver: one sandboxed mod in its own host.
pub struct LuaDriver {
    mods: LuaMods,
}

fn lua_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl LuaDriver {
    fn new(title: &str, script: &str) -> Result<Self, DriverError> {
        let manifest = format!(
            "(format_version: 1, id: {}, name: {}, version: \"1.0.0\", entry: \"main.lua\", api_version: 1)",
            lua_string(LUA_MOD_ID),
            lua_string(title)
        );
        let files = ModFiles::from_map([("luamod.ron", manifest.as_str()), ("main.lua", script)]);
        let mut mods = LuaMods::new(0, LuaLimits::default());
        mods.load(files, &mut NoWorld)
            .map_err(|e| DriverError::load(format!("lua: {e}")))?;
        // Whatever the script emitted while loading is not a command's call.
        let _ = mods.drain_emitted();
        Ok(Self { mods })
    }

    fn run(&mut self, ctx: &PanelContext) -> Result<Vec<PanelCall>, DriverError> {
        let values = ctx
            .values
            .iter()
            .map(|(k, v)| (k.clone(), ScriptValue::String(v.clone())))
            .collect();
        let selection = ctx
            .selection
            .iter()
            .map(|(entity, name)| {
                ScriptValue::Map(BTreeMap::from([
                    ("entity".to_owned(), ScriptValue::Number(*entity as f64)),
                    ("name".to_owned(), ScriptValue::String(name.clone())),
                ]))
            })
            .collect();
        let event = ScriptValue::Map(BTreeMap::from([
            ("command".to_owned(), ScriptValue::String(ctx.command.clone())),
            ("values".to_owned(), ScriptValue::Map(values)),
            ("selection".to_owned(), ScriptValue::List(selection)),
        ]));
        self.mods.fire(LUA_COMMAND_EVENT, event);
        self.mods.deliver_events(&mut NoWorld);
        let emitted = self.mods.drain_emitted();
        if let Some(ModStatus::Disabled { code, message }) = self.mods.get(LUA_MOD_ID).map(|m| m.status()) {
            let error = DriverError::failed(format!("lua ({code}): {message}"));
            // A failed script stays usable: the next command starts from a fresh state.
            let _ = self.mods.restart(LUA_MOD_ID, &mut NoWorld);
            let _ = self.mods.drain_emitted();
            return Err(error);
        }
        let mut calls = Vec::new();
        for event in emitted.into_iter().filter(|e| e.name == LUA_CALL_EVENT) {
            let ScriptValue::Map(mut fields) = event.value else {
                return Err(DriverError::failed(
                    "editor.call needs a table { method, params }",
                ));
            };
            let Some(ScriptValue::String(method)) = fields.remove("method") else {
                return Err(DriverError::failed("editor.call needs a string `method`"));
            };
            let params = match fields.remove("params") {
                None | Some(ScriptValue::Nil) => Value::Object(Map::new()),
                Some(value @ ScriptValue::Map(_)) => script_json(&value),
                Some(ScriptValue::List(list)) if list.is_empty() => Value::Object(Map::new()),
                Some(_) => return Err(DriverError::failed("editor.call `params` must be a table")),
            };
            calls.push(PanelCall { method, params });
        }
        Ok(calls)
    }
}

fn script_json(value: &ScriptValue) -> Value {
    match value {
        ScriptValue::Nil => Value::Null,
        ScriptValue::Bool(b) => json!(b),
        ScriptValue::Number(n) => {
            // Lua 5.1 numbers are doubles; whole numbers go out as integers (entity bits, counts).
            if n.fract() == 0.0 && n.abs() < 9.0e15 {
                json!(*n as i64)
            } else {
                json!(n)
            }
        }
        ScriptValue::String(s) => json!(s),
        ScriptValue::Vec3(v) => json!(v),
        ScriptValue::Entity(bits) => json!(bits),
        ScriptValue::List(items) => Value::Array(items.iter().map(script_json).collect()),
        ScriptValue::Map(fields) => {
            Value::Object(fields.iter().map(|(k, v)| (k.clone(), script_json(v))).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> PanelContext {
        PanelContext {
            command: "rename".into(),
            values: BTreeMap::from([("prefix".to_owned(), "Box_".to_owned())]),
            selection: vec![(7, "Crate".into()), (9, "Barrel".into())],
        }
    }

    #[test]
    fn utility_panel_logic_driver_expands_one_call_per_selected_entity() {
        let text = r#"(
    format_version: 1,
    rules: [(
        name: "rename",
        event: EntityEvent("rename"),
        actions: [(
            task: "editor::Call",
            set: {
                "method": Literal(String("editor.outliner_rename")),
                "for_each_selected": Literal(Bool(true)),
                "name": Literal(String("{prefix}{name}")),
            },
        )],
    )],
)"#;
        let mut driver = PanelDriver::logic(text).expect("compiles");
        let calls = driver.run(&ctx()).expect("runs");
        assert_eq!(
            calls,
            vec![
                PanelCall {
                    method: "editor.outliner_rename".into(),
                    params: json!({"entity": 7, "name": "Box_Crate"}),
                },
                PanelCall {
                    method: "editor.outliner_rename".into(),
                    params: json!({"entity": 9, "name": "Box_Barrel"}),
                },
            ]
        );
        // Another command name fires no rule.
        let mut other = ctx();
        other.command = "other".into();
        assert!(driver.run(&other).expect("runs").is_empty());
        // A template naming no panel value fails the command, not the editor.
        let bad = text.replace("{prefix}", "{missing}");
        let err = PanelDriver::logic(&bad)
            .expect("compiles")
            .run(&ctx())
            .expect_err("refused");
        assert_eq!(err.code, "utility_panel_driver_failed");
        // An unknown task is a load error.
        let unknown = text.replace("editor::Call", "editor::Nope");
        let err = PanelDriver::logic(&unknown).err().expect("refused");
        assert_eq!(err.code, "utility_panel_driver_load");
    }

    #[test]
    fn utility_panel_lua_driver_answers_calls_from_the_context() {
        let script = r#"
regia.on("command", function(ctx)
    if ctx.command ~= "rename" then return end
    for _, item in ipairs(ctx.selection) do
        regia.emit("editor.call", { method = "editor.outliner_rename",
            params = { entity = item.entity, name = ctx.values.prefix .. item.name } })
    end
end)
"#;
        let mut driver = PanelDriver::lua("Rename", script).expect("loads");
        let calls = driver.run(&ctx()).expect("runs");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].method, "editor.outliner_rename");
        assert_eq!(calls[0].params, json!({"entity": 7, "name": "Box_Crate"}));
        assert_eq!(calls[1].params, json!({"entity": 9, "name": "Box_Barrel"}));
        // A runtime error fails the command and the next command runs again.
        let failing = r#"regia.on("command", function(ctx) error("boom") end)"#;
        let mut driver = PanelDriver::lua("Fail", failing).expect("loads");
        let err = driver.run(&ctx()).expect_err("refused");
        assert_eq!(err.code, "utility_panel_driver_failed");
        assert!(err.message.contains("boom"), "{}", err.message);
        assert_eq!(
            driver.run(&ctx()).expect_err("refused").code,
            "utility_panel_driver_failed"
        );
        // The sandbox has no host access.
        let escape = r#"regia.on("command", function(ctx) os.execute("true") end)"#;
        let err = PanelDriver::lua("Escape", escape)
            .expect("loads")
            .run(&ctx())
            .expect_err("refused");
        assert_eq!(err.code, "utility_panel_driver_failed");
        assert_eq!(
            PanelDriver::lua("Broken", "this is not lua")
                .err()
                .expect("refused")
                .code,
            "utility_panel_driver_load"
        );
    }

    #[test]
    fn utility_panel_templates_keep_literal_braces() {
        let c = ctx();
        assert_eq!(
            template(&json!("{{x}} {prefix}{name}"), &c, Some("A")).expect("ok"),
            json!("{x} Box_A")
        );
        assert!(template(&json!("{prefix"), &c, None).is_err());
        assert_eq!(template(&json!(3), &c, None).expect("ok"), json!(3));
    }
}
