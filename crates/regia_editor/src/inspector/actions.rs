// SPDX-License-Identifier: MPL-2.0
//! Inspector buttons and component actions (UI parity addendum § 7.9, ADR-7785).
//!
//! A component action is editor code that runs on one entity's component: "Bake", "Rebuild",
//! "Snap to ground". Game or tool code registers it in [`ComponentActions`] (directly on the
//! editor world, or through the extension context's
//! [`register_component_action`](crate::extension::context::ToolExtensionContext::register_component_action));
//! no method is reflected. The Inspector shows it two ways:
//!
//! - a field with `@Button("Bake")` (usually a unit marker such as `bake: ()`) is drawn as a
//!   button that runs the action whose id is the field's name;
//! - every other registered action of the component is a button in the component's action row.
//!
//! Running an action is one undo step: the component's reflected value before and after the run
//! is recorded as a whole-component `SetProperty`, so undo restores what the action changed on
//! that component (changes to other components or entities are the action's own business). A run
//! that fails restores the component and reports the action's message.
//!
//! Agents list the actions with `editor.component_actions { entity }` and run one with
//! `editor.run_component_action { entity, component, action }` (`book/agents/inspector.md`).

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::attrs::EditorAttrs;
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, TypeInfo};
use serde_json::{Value, json};

use crate::command::{EditorCommand as _, SetProperty};

/// Runs an action on one entity; `Err` carries the message shown to the user or agent.
pub type ActionFn = Arc<dyn Fn(&mut World, Entity) -> Result<(), String> + Send + Sync>;

/// One component action.
#[derive(Clone)]
pub struct ComponentAction {
    /// The component's type path or short name (`"Lamp"`, `"game::Lamp"`).
    pub component: String,
    /// The action's id: a `@Button` field's name, or any identifier.
    pub id: String,
    /// The button text (`"Bake"`).
    pub label: String,
    /// What it does.
    pub run: ActionFn,
}

impl std::fmt::Debug for ComponentAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComponentAction")
            .field("component", &self.component)
            .field("id", &self.id)
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

/// The short name of a type path (`game::Lamp` → `Lamp`).
fn short(component: &str) -> &str {
    component.rsplit("::").next().unwrap_or(component)
}

/// The editor's component actions, by (component short name, action id), with the registering
/// owner (an extension id, or `"game"`).
#[derive(Resource, Default, Debug)]
pub struct ComponentActions {
    actions: BTreeMap<(String, String), (String, ComponentAction)>,
}

impl ComponentActions {
    /// Registers `action` for `owner`. The same owner registering the same action again replaces
    /// it (a reload).
    ///
    /// # Errors
    ///
    /// The action is registered by another owner, or its id is empty.
    pub fn register(&mut self, owner: &str, action: ComponentAction) -> Result<(), String> {
        if action.id.trim().is_empty() || action.component.trim().is_empty() {
            return Err("a component action needs a component and an id".into());
        }
        let key = (short(&action.component).to_owned(), action.id.clone());
        if let Some((existing, _)) = self.actions.get(&key)
            && existing != owner
        {
            return Err(format!("{}.{} is registered by {existing}", key.0, key.1));
        }
        self.actions.insert(key, (owner.to_owned(), action));
        Ok(())
    }

    /// Removes every action `owner` registered.
    pub fn remove_owner(&mut self, owner: &str) {
        self.actions.retain(|_, (o, _)| o != owner);
    }

    /// The action `id` of `component` (type path or short name).
    #[must_use]
    pub fn get(&self, component: &str, id: &str) -> Option<&ComponentAction> {
        self.actions
            .get(&(short(component).to_owned(), id.to_owned()))
            .map(|(_, action)| action)
    }

    /// The actions of `component`, by id.
    pub fn for_component<'a>(&'a self, component: &'a str) -> impl Iterator<Item = &'a ComponentAction> + 'a {
        let short = short(component);
        self.actions
            .iter()
            .filter(move |((c, _), _)| c == short)
            .map(|(_, (_, action))| action)
    }
}

/// One action an entity's component offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionEntry {
    /// The component's type path.
    pub component: String,
    /// The action id.
    pub action: String,
    /// The button text.
    pub label: String,
    /// `"button"` (a `@Button` field) or `"registered"` (only registered).
    pub source: &'static str,
    /// Whether an action is registered for it (a `@Button` without one does nothing yet).
    pub runnable: bool,
}

/// The `@Button` fields of a component type: `(field name, label)`.
#[must_use]
pub fn button_fields(info: &TypeInfo) -> Vec<(String, &'static str)> {
    let TypeInfo::Struct(info) = info else {
        return Vec::new();
    };
    info.iter()
        .filter_map(|field| {
            EditorAttrs::from_custom(field.custom_attributes())
                .button
                .map(|label| (field.name().to_owned(), label))
        })
        .collect()
}

/// Every action of every reflected component on `entity`: `@Button` fields first (in field
/// order), then the other registered actions, components sorted by type path.
#[must_use]
pub fn entity_actions(world: &World, entity: Entity) -> Vec<ActionEntry> {
    let Ok(infos) = world.inspect_entity(entity) else {
        return Vec::new();
    };
    let mut components: Vec<String> = infos.map(|(_, info)| info.name().to_string()).collect();
    components.sort();
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let registered = world.get_resource::<ComponentActions>();
    let mut out = Vec::new();
    for component in components {
        let Some(registration) = registry.get_with_type_path(&component) else {
            continue;
        };
        let buttons = button_fields(registration.type_info());
        for (field, label) in &buttons {
            out.push(ActionEntry {
                component: component.clone(),
                action: field.clone(),
                label: (*label).to_owned(),
                source: "button",
                runnable: registered.is_some_and(|r| r.get(&component, field).is_some()),
            });
        }
        if let Some(registered) = registered {
            for action in registered.for_component(&component) {
                if buttons.iter().any(|(field, _)| *field == action.id) {
                    continue;
                }
                out.push(ActionEntry {
                    component: component.clone(),
                    action: action.id.clone(),
                    label: action.label.clone(),
                    source: "registered",
                    runnable: true,
                });
            }
        }
    }
    out
}

/// A refused or failed action run, with its agent error code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionError {
    /// `component_absent`, `action_unknown` or `action_failed`.
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

/// What a run did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionRun {
    /// Whether the component's value changed (and an undo entry was pushed).
    pub changed: bool,
    /// The undo depth after the run.
    pub undoable: usize,
}

fn refuse(code: &'static str, message: impl Into<String>) -> ActionError {
    ActionError {
        code,
        message: message.into(),
    }
}

/// The full type path of `component` (type path or short name) if `entity` has it.
fn component_on(world: &World, entity: Entity, component: &str) -> Option<String> {
    let infos = world.inspect_entity(entity).ok()?;
    let mut fallback = None;
    for (_, info) in infos {
        let name = info.name().to_string();
        if name == component {
            return Some(name);
        }
        if short(&name) == short(component) && fallback.is_none() {
            fallback = Some(name);
        }
    }
    fallback
}

/// Runs action `action` of `component` on `entity` as one undo step (module docs).
///
/// # Errors
///
/// `component_absent`, `action_unknown`, or `action_failed` (the component is restored).
pub fn run_component_action(
    world: &mut World,
    entity: Entity,
    component: &str,
    action: &str,
) -> Result<ActionRun, ActionError> {
    let _span = tracing::info_span!("component_action", component, action).entered();
    let Some(full) = component_on(world, entity, component) else {
        return Err(refuse(
            "component_absent",
            format!("entity {} has no {component}", entity.to_bits()),
        ));
    };
    let Some(run) = world
        .get_resource::<ComponentActions>()
        .and_then(|actions| actions.get(&full, action))
        .map(|a| Arc::clone(&a.run))
    else {
        return Err(refuse(
            "action_unknown",
            format!(
                "{} has no action {action:?}; see editor.component_actions",
                short(&full)
            ),
        ));
    };
    let before = crate::command::read_property(world, entity, &full, "")
        .map_err(|e| refuse("component_absent", e.to_string()))?;
    let restore = |world: &mut World, value: &dyn regia_ecs::reflect::PartialReflect| {
        if let Some(copy) = super::reflect_ui::clone_value(value)
            && let Ok(mut command) = SetProperty::capture(world, entity, &full, "", copy)
        {
            command.apply(world);
        }
    };
    if let Err(message) = run(world, entity) {
        if world.get_entity(entity).is_ok() {
            restore(world, before.as_ref());
        }
        return Err(refuse("action_failed", message));
    }
    let depth = |world: &World| {
        world
            .get_resource::<crate::command::UndoStack>()
            .map_or(0, crate::command::UndoStack::undo_depth)
    };
    let Ok(after) = crate::command::read_property(world, entity, &full, "") else {
        // The action removed the component (or the entity): nothing to record on it.
        return Ok(ActionRun {
            changed: true,
            undoable: depth(world),
        });
    };
    if before.reflect_partial_eq(after.as_ref()).unwrap_or(false) {
        return Ok(ActionRun {
            changed: false,
            undoable: depth(world),
        });
    }
    // Put the old value back so the command captures it, then apply the action's result.
    restore(world, before.as_ref());
    let mut command = SetProperty::capture(world, entity, &full, "", after)
        .map_err(|e| refuse("action_failed", e.to_string()))?;
    command.coalesce = false;
    command.apply(world);
    let undoable = crate::undo_agent::push_discrete(world, Box::new(command));
    Ok(ActionRun {
        changed: true,
        undoable,
    })
}

/// Draws the registered actions of `component` that have no `@Button` field as a row of buttons;
/// returns the id of the one clicked.
pub fn action_row(
    ui: &mut egui::Ui,
    world: &World,
    component: &str,
    type_info: Option<&TypeInfo>,
) -> Option<String> {
    let actions = world.get_resource::<ComponentActions>()?;
    let buttons = type_info.map(button_fields).unwrap_or_default();
    let listed: Vec<&ComponentAction> = actions
        .for_component(component)
        .filter(|action| !buttons.iter().any(|(field, _)| *field == action.id))
        .collect();
    if listed.is_empty() {
        return None;
    }
    let mut clicked = None;
    ui.horizontal_wrapped(|ui| {
        for action in listed {
            if ui
                .button(&action.label)
                .on_hover_text(format!("Run {} on this entity (undoable)", action.label))
                .clicked()
            {
                clicked = Some(action.id.clone());
            }
        }
    });
    clicked
}

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

fn rpc(err: ActionError) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        err.code,
        err.message,
        "editor.component_actions lists what the entity's components offer",
    )
}

fn text_param(params: &Value, name: &str) -> Result<String, RpcError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` (string) is required")))
}

/// `editor.component_actions { entity }`.
fn component_actions(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::undo_agent::live_entity(world, params, "entity")?;
    let actions: Vec<Value> = entity_actions(world, entity)
        .into_iter()
        .map(|a| {
            json!({
                "component": a.component,
                "action": a.action,
                "label": a.label,
                "source": a.source,
                "runnable": a.runnable,
            })
        })
        .collect();
    Ok(json!({ "entity": entity.to_bits(), "actions": actions }))
}

/// `editor.run_component_action { entity, component, action }`.
fn run_action(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::undo_agent::live_entity(world, params, "entity")?;
    regia_agent::require_not_feed_owned(world, entity)?;
    let component = text_param(params, "component")?;
    let action = text_param(params, "action")?;
    let run = run_component_action(world, entity, &component, &action).map_err(rpc)?;
    Ok(json!({
        "entity": entity.to_bits(),
        "component": component,
        "action": action,
        "changed": run.changed,
        "undoable": run.undoable,
    }))
}

/// The component action agent methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.component_actions",
                tool: "component_actions",
                description: "The actions an entity's components offer in the Inspector: `@Button` fields (source \"button\"; runnable when an action is registered for the field) and actions editor or extension code registered (source \"registered\").",
                params: &[ParamSchema {
                    name: "entity",
                    kind: "number",
                    required: true,
                    description: "Entity id (from scene.query_entities).",
                }],
                result: "{ entity, actions: [{ component, action, label, source, runnable }] }",
                mutating: false,
                permission: Permission::Read,
            },
            component_actions as Handler,
        ),
        (
            MethodSchema {
                name: "editor.run_component_action",
                tool: "run_component_action",
                description: "Run a component action on an entity, as the Inspector's button does: one undo step that restores the component's value (editor.undo). A failing action restores the component. Errors: component_absent, action_unknown, action_failed.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: true,
                        description: "Entity id.",
                    },
                    ParamSchema {
                        name: "component",
                        kind: "string",
                        required: true,
                        description: "Component type name, short or full (\"Lamp\").",
                    },
                    ParamSchema {
                        name: "action",
                        kind: "string",
                        required: true,
                        description: "The action id from editor.component_actions.",
                    },
                ],
                result: "{ entity, component, action, changed, undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            run_action as Handler,
        ),
    ]
}
