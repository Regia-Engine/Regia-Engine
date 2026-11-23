// SPDX-License-Identifier: MPL-2.0
//! Saved, undoable review markers and their keyboard-accessible Notes panel (ADR-7841).

use crate::command::{EditorCommand, SetProperty, SpawnEntities, UndoStack};
use crate::selection::{SelectMode, Selection};
use crate::viewport_agent::Handler;
use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::{id::Guid, math::Vec3};
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, ReflectComponent};
use regia_scene::prelude::{EntityGuid, Transform};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A reply in a saved review thread. Times are Unix milliseconds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Reflect, Serialize, Deserialize)]
pub struct Reply {
    /// Reply body, at most 16 KiB.
    pub text: String,
    /// Author display name.
    pub author: String,
    /// Creation time in Unix milliseconds.
    pub created: u64,
}

/// Editor annotation on a dedicated marker entity; saved in levels and stripped by cooking.
#[derive(Component, Debug, Clone, Default, PartialEq, Eq, Reflect, Serialize, Deserialize)]
#[reflect(Component, Default)]
pub struct ReviewNote {
    /// Note body, at most 16 KiB.
    pub text: String,
    /// Author display name.
    pub author: String,
    /// Creation time in Unix milliseconds.
    pub created: u64,
    /// Whether the discussion is resolved.
    pub resolved: bool,
    /// Replies in creation order, at most 1,000.
    pub thread: Vec<Reply>,
}

#[derive(Resource, Default)]
struct NotesPanel {
    draft: String,
    reply: String,
    filter: String,
    selected: Option<u64>,
    placement: Option<[f32; 3]>,
    cursor_ray: Option<(Vec3, Vec3, f32)>,
    cursor_owner: Option<(u64, usize)>,
    last_action: Option<u64>,
    error: String,
}

#[derive(Resource, Default)]
struct ReviewAuthor {
    root: Option<std::path::PathBuf>,
    git_name: Option<String>,
}

/// Registers the editor-owned component, without adding editor dependencies to runtime crates.
pub fn install(world: &mut World) {
    let registry = world
        .get_resource_or_insert_with(AppTypeRegistry::default)
        .clone();
    let mut registry = registry.write();
    registry.register::<Reply>();
    registry.register::<ReviewNote>();
}

fn error(code: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, "See book/agents/review-notes.md")
}

fn body(params: &Value) -> Result<String, RpcError> {
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| error("note_text", "text must be a string"))?;
    if text.trim().is_empty() || text.len() > 16 * 1024 {
        return Err(error(
            "note_text",
            "text must contain 1 to 16384 bytes of nonblank text",
        ));
    }
    Ok(text.to_owned())
}

/// Git's project user name, falling back to the user's review.author preference.
pub fn author(world: &mut World) -> String {
    let root = crate::mesh_source::project_root(world);
    if world
        .get_resource::<ReviewAuthor>()
        .is_none_or(|cached| cached.root != root)
    {
        let git_name = root.as_ref().and_then(|root| {
            std::process::Command::new("git")
                .current_dir(root)
                .args(["config", "--get", "user.name"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty())
        });
        world.insert_resource(ReviewAuthor { root, git_name });
    }
    world
        .get_resource::<ReviewAuthor>()
        .and_then(|author| author.git_name.clone())
        .unwrap_or_else(|| {
            world
                .get_resource::<crate::editor_prefs::EditorPreferences>()
                .map(|prefs| prefs.text("review.author"))
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| "Reviewer".to_owned())
        })
}

fn note_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| error("note_entity", "entity must be an unsigned integer"))?;
    crate::command::resolve_entity(world, bits)
        .filter(|entity| world.get::<ReviewNote>(*entity).is_some())
        .ok_or_else(|| {
            RpcError::new(
                NOT_FOUND,
                "note_not_found",
                "Review note not found",
                "Call editor.note_list",
            )
        })
}

fn apply(world: &mut World, mut command: impl EditorCommand + 'static) {
    command.apply(world);
    let mut stack = world.get_resource_or_insert_with(UndoStack::new);
    stack.seal();
    stack.push(Box::new(command));
    stack.seal();
}

fn note_add(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = body(params)?;
    let level = world
        .get_resource::<regia_scene::prelude::LevelManager>()
        .and_then(|manager| manager.current())
        .ok_or_else(|| error("note_level", "Open a level before adding a review note"))?;
    if params.get("entity").is_some() && params.get("position").is_some() {
        return Err(error("note_placement", "Use entity or position, not both"));
    }
    let parent = match params.get("entity") {
        Some(value) => Some(
            value
                .as_u64()
                .and_then(|bits| crate::command::resolve_entity(world, bits))
                .filter(|entity| world.get::<EntityGuid>(*entity).is_some())
                .ok_or_else(|| error("note_entity", "Attachment entity does not exist in the level"))?,
        ),
        None if params.get("position").is_none() => {
            world.get_resource::<Selection>().and_then(Selection::primary)
        }
        None => None,
    };
    if parent.is_some_and(|entity| {
        world
            .get::<regia_scene::level::LevelMember>(entity)
            .is_none_or(|member| member.level() != level)
    }) {
        return Err(error("note_entity", "Attachment must belong to the open level"));
    }
    if parent.is_some_and(|entity| world.get::<ReviewNote>(entity).is_some()) {
        return Err(error(
            "note_placement",
            "Attach to scene content, not another note",
        ));
    }
    let position = if let Some(value) = params.get("position") {
        let values = value
            .as_array()
            .filter(|v| v.len() == 3)
            .ok_or_else(|| error("note_placement", "position must be [x,y,z]"))?;
        let mut position = [0.0; 3];
        for (index, value) in values.iter().enumerate() {
            position[index] = value.as_f64().unwrap_or(f64::NAN) as f32;
        }
        let position = Vec3::from_array(position);
        if !position.is_finite() {
            return Err(error("note_placement", "position must be finite"));
        }
        position
    } else if parent.is_some() {
        Vec3::ZERO
    } else {
        return Err(error(
            "note_placement",
            "Select an entity or supply a surface position",
        ));
    };
    let note = ReviewNote {
        text,
        author: author(world),
        created: regia_agent::unix_ms_now(),
        ..Default::default()
    };
    install(world);
    let guid = Guid::new_random();
    let transform = Transform {
        translation: position,
        ..Default::default()
    };
    let mut spawn = SpawnEntities::new_entity(world, guid, "Review note", transform)
        .and_then(|command| command.with_component(world, &note))
        .map_err(|err| error("note_edit", err.to_string()))?;
    spawn.apply(world);
    let entity = world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .find_map(|(entity, id)| (id.0 == guid).then_some(entity))
        .ok_or_else(|| error("note_edit", "Could not spawn review marker"))?;
    let mut group = crate::command::CommandGroup::new("Add review note");
    group.push(Box::new(spawn));
    if let Some(parent) = parent {
        let mut reparent = crate::command::Reparent::capture(world, entity, Some(parent));
        reparent.apply(world);
        group.push(Box::new(reparent));
    }
    let mut stack = world.get_resource_or_insert_with(UndoStack::new);
    stack.seal();
    stack.push(Box::new(group));
    stack.seal();
    Ok(json!({"entity": entity.to_bits(), "note": note}))
}

fn replace(world: &mut World, entity: Entity, note: ReviewNote) -> Result<Value, RpcError> {
    let mut command = SetProperty::capture(
        world,
        entity,
        "regia_editor::review_notes::ReviewNote",
        "",
        Box::new(note.clone()),
    )
    .map_err(|err| error("note_edit", err.to_string()))?;
    command.coalesce = false;
    apply(world, command);
    Ok(json!({"entity": entity.to_bits(), "note": note}))
}

fn note_reply(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = body(params)?;
    let entity = note_entity(world, params)?;
    let mut note = world
        .get::<ReviewNote>(entity)
        .cloned()
        .ok_or_else(|| error("note_not_found", "Note disappeared"))?;
    if note.thread.len() >= 1000 {
        return Err(error("note_thread_full", "A note may have at most 1000 replies"));
    }
    note.thread.push(Reply {
        text,
        author: author(world),
        created: regia_agent::unix_ms_now(),
    });
    replace(world, entity, note)
}

fn note_resolve(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = note_entity(world, params)?;
    let resolved = params
        .get("resolved")
        .and_then(Value::as_bool)
        .ok_or_else(|| error("note_resolved", "resolved must be a boolean"))?;
    let mut note = world
        .get::<ReviewNote>(entity)
        .cloned()
        .ok_or_else(|| error("note_not_found", "Note disappeared"))?;
    note.resolved = resolved;
    replace(world, entity, note)
}

fn note_list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let filter = match params.get("filter") {
        None => "all",
        Some(value) => value
            .as_str()
            .ok_or_else(|| error("note_filter", "filter must be text"))?,
    };
    if !["all", "open", "resolved", "mine"].contains(&filter) {
        return Err(error("note_filter", "filter must be all, open, resolved or mine"));
    }
    let mine = author(world);
    let mut notes: Vec<Value> = world
        .query::<(Entity, &ReviewNote)>()
        .iter(world)
        .filter(|(_, note)| match filter {
            "open" => !note.resolved,
            "resolved" => note.resolved,
            "mine" => note.author == mine,
            _ => true,
        })
        .map(|(entity, note)| {
            json!({"entity": entity.to_bits(), "note": note,
            "position": crate::viewport::world_position(world, entity).map(|position| position.to_array())})
        })
        .collect();
    notes.sort_by_key(|row| (row["note"]["created"].as_u64(), row["entity"].as_u64()));
    Ok(json!({"notes": notes}))
}

/// The four review-note protocol methods; GUI controls use the same handlers.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.note_add",
                tool: "editor_note_add",
                description: "Add an undoable review marker to the level.",
                params: &[
                    ParamSchema {
                        name: "text",
                        kind: "string",
                        required: true,
                        description: "Nonblank body, at most 16384 bytes.",
                    },
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: false,
                        description: "Attach a new marker to this entity; otherwise use the selection.",
                    },
                    ParamSchema {
                        name: "position",
                        kind: "array",
                        required: false,
                        description: "World surface position [x,y,z], instead of an attachment.",
                    },
                ],
                result: "{entity, note}",
                mutating: true,
                permission: Permission::EditScene,
            },
            note_add as Handler,
        ),
        (
            MethodSchema {
                name: "editor.note_reply",
                tool: "editor_note_reply",
                description: "Reply to a review note, undoable.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: true,
                        description: "Review marker entity id.",
                    },
                    ParamSchema {
                        name: "text",
                        kind: "string",
                        required: true,
                        description: "Nonblank body, at most 16384 bytes.",
                    },
                ],
                result: "{entity, note}",
                mutating: true,
                permission: Permission::EditScene,
            },
            note_reply as Handler,
        ),
        (
            MethodSchema {
                name: "editor.note_resolve",
                tool: "editor_note_resolve",
                description: "Resolve or reopen a review note, undoable.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: true,
                        description: "Review marker entity id.",
                    },
                    ParamSchema {
                        name: "resolved",
                        kind: "boolean",
                        required: true,
                        description: "true resolves; false reopens.",
                    },
                ],
                result: "{entity, note}",
                mutating: true,
                permission: Permission::EditScene,
            },
            note_resolve as Handler,
        ),
        (
            MethodSchema {
                name: "editor.note_list",
                tool: "editor_note_list",
                description: "List saved review notes and their replies.",
                params: &[ParamSchema {
                    name: "filter",
                    kind: "string",
                    required: false,
                    description: "all (default), open, resolved or mine.",
                }],
                result: "{notes: [{entity, note, position}]}",
                mutating: false,
                permission: Permission::Read,
            },
            note_list as Handler,
        ),
    ]
}

/// Opens the Notes draft at a cursor surface, or attached to the current selection.
pub fn begin_note(world: &mut World) {
    let ray = world
        .get_resource::<NotesPanel>()
        .and_then(|panel| panel.cursor_ray);
    let position = ray
        .and_then(|(origin, direction, far)| crate::viewport_ops::cpu_pick(world, origin, direction, far))
        .map(|(_, position)| position.to_array());
    world.get_resource_or_insert_with(NotesPanel::default).placement = position;
    crate::editor_layout::LayoutRequests::push(
        world,
        crate::editor_layout::LayoutRequest::Open(crate::shell::Panel::Notes),
    );
}

/// Selects and frames a marker through the existing camera command.
pub fn frame_note(world: &mut World, bits: u64) {
    if let Some(entity) = crate::command::resolve_entity(world, bits) {
        world
            .get_resource_or_insert_with(Selection::default)
            .select(entity, SelectMode::Replace);
        let camera = world
            .get_resource_or_insert_with(crate::viewport::EditorCamera::default)
            .clone();
        let to = crate::viewport_ops::focus_pose(world, &camera, &[entity]);
        world
            .get_resource_or_insert_with(crate::viewport_ops::CameraEases::default)
            .start(crate::viewport_ops::MAIN_CAMERA, camera, to);
        world.get_resource_or_insert_with(NotesPanel::default).selected = Some(entity.to_bits());
    }
}

/// Notes tab. All actions are normal focusable, named buttons; scale comes from egui preferences.
#[tracing::instrument(skip_all)]
pub fn panel(ui: &mut egui::Ui, world: &mut World) {
    use crate::agent_parity as parity;
    let mine = author(world);
    let mut panel = world.remove_resource::<NotesPanel>().unwrap_or_default();
    if panel.filter.is_empty() {
        panel.filter = "all".to_owned();
    }
    let frame = ui.ctx().cumulative_frame_nr();
    let available = panel.last_action != Some(frame);
    ui.label(format!("Author: {mine}"));
    ui.horizontal(|ui| {
        ui.label("Show");
        for filter in ["all", "open", "resolved", "mine"] {
            ui.selectable_value(&mut panel.filter, filter.to_owned(), filter);
        }
    });
    let notes = note_list(world, &json!({"filter": panel.filter})).unwrap_or_else(|_| json!({"notes": []}));
    let mut action: Option<(&str, Handler, Value)> = None;
    let label = ui.label("New note");
    ui.add(egui::TextEdit::multiline(&mut panel.draft).hint_text("Review note text"))
        .labelled_by(label.id);
    if parity::action_enabled(
        ui,
        available && !panel.draft.trim().is_empty(),
        &parity::NOTE_ADD,
        None,
    )
    .clicked()
    {
        let mut params = json!({"text": panel.draft});
        if let Some(position) = panel.placement {
            params["position"] = json!(position);
        }
        action = Some(("add", note_add, params));
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        for row in notes["notes"].as_array().into_iter().flatten() {
            let Some(bits) = row["entity"].as_u64() else {
                continue;
            };
            ui.push_id(bits, |ui| {
                let note = &row["note"];
                let label = format!(
                    "{}: {} ({} replies)",
                    note["author"].as_str().unwrap_or(""),
                    note["text"].as_str().unwrap_or(""),
                    note["thread"].as_array().map_or(0, Vec::len)
                );
                if parity::action_button_text(ui, &parity::NOTE_FRAME, label).clicked() && available {
                    frame_note(world, bits);
                    panel.selected = Some(bits);
                    panel.last_action = Some(frame);
                }
                if panel.selected == Some(bits) {
                    for reply in note["thread"].as_array().into_iter().flatten() {
                        ui.label(format!(
                            "{}: {}",
                            reply["author"].as_str().unwrap_or(""),
                            reply["text"].as_str().unwrap_or("")
                        ));
                    }
                    let label = ui.label("Reply text");
                    ui.add(egui::TextEdit::multiline(&mut panel.reply).hint_text("Reply text"))
                        .labelled_by(label.id);
                    if parity::action_enabled(
                        ui,
                        available && !panel.reply.trim().is_empty(),
                        &parity::NOTE_REPLY,
                        None,
                    )
                    .clicked()
                    {
                        action = Some(("reply", note_reply, json!({"entity": bits, "text": panel.reply})));
                    }
                    let resolved = note["resolved"].as_bool().unwrap_or(false);
                    let control = if resolved {
                        &parity::NOTE_REOPEN
                    } else {
                        &parity::NOTE_RESOLVE
                    };
                    if parity::action_enabled(ui, available, control, None).clicked() {
                        action = Some((
                            "resolve",
                            note_resolve,
                            json!({"entity": bits, "resolved": !resolved}),
                        ));
                    }
                }
            });
        }
    });
    if let Some((kind, handler, params)) = action {
        panel.last_action = Some(frame);
        match handler(world, &params) {
            Ok(result) => {
                panel.error.clear();
                if kind == "add" {
                    panel.draft.clear();
                }
                if kind == "reply" {
                    panel.reply.clear();
                }
                panel.selected = result["entity"].as_u64();
            }
            Err(err) => panel.error = err.message,
        }
    }
    if !panel.error.is_empty() {
        ui.colored_label(crate::theme::tokens(world).accents.error_text, &panel.error);
    }
    world.insert_resource(panel);
}

/// Draws world-space note badges and remembers the surface beneath the viewport cursor.
#[tracing::instrument(skip_all)]
pub fn pins(
    ui: &mut egui::Ui,
    world: &mut World,
    view: &crate::views::ViewCamera,
    rect: egui::Rect,
    pane: (u64, usize),
) -> bool {
    let mut owns_pointer = false;
    let size = regia_core::math::Vec2::new(rect.width(), rect.height());
    if let Some(pointer) = ui
        .input(|input| input.pointer.hover_pos())
        .filter(|point| rect.contains(*point))
    {
        let pixel = pointer - rect.min;
        let (origin, direction) = view.ray(regia_core::math::Vec2::new(pixel.x, pixel.y), size);
        let mut panel = world.get_resource_or_insert_with(NotesPanel::default);
        panel.cursor_ray = Some((origin, direction, view.far));
        panel.cursor_owner = Some(pane);
    } else if let Some(mut panel) = world.get_resource_mut::<NotesPanel>()
        && panel.cursor_owner == Some(pane)
    {
        // Leaving this viewport must not place the next keyboard note at a stale surface.
        // Another viewport's ray is retained when several panes draw in the same frame.
        panel.cursor_ray = None;
        panel.cursor_owner = None;
    }
    let notes: Vec<_> = world
        .query::<(Entity, &ReviewNote)>()
        .iter(world)
        .filter_map(|(entity, note)| {
            let position = crate::viewport::world_position(world, entity)?;
            Some((
                entity.to_bits(),
                view.project(position, size)?,
                note.thread.len() + 1,
                note.resolved,
            ))
        })
        .collect();
    for (bits, at, count, resolved) in notes {
        let color = if resolved {
            crate::theme::tokens(world).accents.ok
        } else {
            crate::theme::tokens(world).accents.active
        };
        let label = egui::RichText::new(format!("Note {count}")).color(color);
        let position = rect.min + egui::vec2(at.x, at.y);
        if !rect.contains(position) {
            continue;
        }
        let badge = egui::Rect::from_min_size(position, egui::vec2(65.0, ui.spacing().interact_size.y));
        ui.scope_builder(
            egui::UiBuilder::new()
                .id_salt(("review_pin", bits))
                .max_rect(badge),
            |ui| {
                ui.set_clip_rect(ui.clip_rect().intersect(rect));
                let response =
                    crate::agent_parity::action_button_text(ui, &crate::agent_parity::NOTE_FRAME, label);
                owns_pointer |= response.contains_pointer() || response.clicked();
                let frame = ui.ctx().cumulative_frame_nr();
                if response.clicked()
                    && world
                        .get_resource::<NotesPanel>()
                        .is_none_or(|panel| panel.last_action != Some(frame))
                {
                    frame_note(world, bits);
                    world.get_resource_or_insert_with(NotesPanel::default).last_action = Some(frame);
                    crate::editor_layout::LayoutRequests::push(
                        world,
                        crate::editor_layout::LayoutRequest::Open(crate::shell::Panel::Notes),
                    );
                }
            },
        );
    }
    owns_pointer
}

#[cfg(test)]
mod tests {
    use regia_app::prelude::*;
    use serde_json::json;

    #[test]
    fn review_notes_agent_contract_exists() {
        let mut world = World::new();
        crate::agent_ext::install(&mut world);
        let methods = crate::agent_ext::all_methods();
        for name in [
            "editor.note_add",
            "editor.note_reply",
            "editor.note_resolve",
            "editor.note_list",
        ] {
            assert!(
                methods.iter().any(|(schema, _)| schema.name == name),
                "missing {name}"
            );
        }
        let (_, list) = methods
            .iter()
            .find(|(schema, _)| schema.name == "editor.note_list")
            .expect("list");
        assert_eq!(list(&mut world, &json!({})).expect("list")["notes"], json!([]));
    }

    #[test]
    fn review_notes_author_pref_and_mine_filter_follow_current_identity() {
        let mut agent = super::acceptance::Agent::new();
        // An unavailable git name falls back to the preference; no global git config is changed.
        agent
            .app
            .world_mut()
            .insert_resource(super::ReviewAuthor::default());
        agent.call(
            "editor.prefs_set",
            json!({"values":{"review.author":"Reviewer A"}}),
        );
        agent.call("editor.note_add", json!({"text":"Mine","position":[0,0,0]}));
        let mine = agent.call("editor.note_list", json!({"filter":"mine"}));
        assert_eq!(mine["notes"][0]["note"]["author"], "Reviewer A");
        agent.call(
            "editor.prefs_set",
            json!({"values":{"review.author":"Reviewer B"}}),
        );
        assert_eq!(
            agent.call("editor.note_list", json!({"filter":"mine"}))["notes"],
            json!([])
        );
        agent.app.world_mut().insert_resource(super::ReviewAuthor {
            git_name: Some("Git reviewer".into()),
            ..Default::default()
        });
        assert_eq!(super::author(agent.app.world_mut()), "Git reviewer");
        let other_project = regia_test_utils::TempDir::new("review-missing-project");
        agent
            .app
            .world_mut()
            .insert_resource(crate::agent_ext::EditorProject {
                root: Some(other_project.join("absent")),
            });
        assert_eq!(
            super::author(agent.app.world_mut()),
            "Reviewer B",
            "a different project cannot inherit the previous git identity"
        );
    }
}

#[cfg(test)]
#[path = "review_notes/tests.rs"]
pub(crate) mod acceptance;

#[cfg(test)]
mod keyboard_tests {
    use super::*;

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn draw(ctx: &egui::Context, world: &mut World, events: Vec<egui::Event>, double: bool) -> String {
        let mut passes = 0;
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 700.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                passes += 1;
                egui::CentralPanel::default().show(ui, |ui| panel(ui, world));
                if double && passes == 1 {
                    ui.ctx().request_discard("exercise second pass");
                }
            },
        );
        output.textures_delta.clear();
        if double {
            assert!(passes >= 2, "the fixture actually repeats the UI frame");
        }
        let update = output.platform_output.accesskit_update.expect("accessible tree");
        assert!(crate::a11y_audit::unnamed_widgets(&update).is_empty());
        update
            .nodes
            .iter()
            .find(|(id, _)| *id == update.focus)
            .and_then(|(_, node)| node.label())
            .unwrap_or_default()
            .to_owned()
    }

    fn tab_to(ctx: &egui::Context, world: &mut World, name: &str) {
        draw(ctx, world, vec![], false);
        for _ in 0..20 {
            if draw(ctx, world, vec![key(egui::Key::Tab)], false) == name {
                return;
            }
        }
        panic!("Tab never reached {name}");
    }

    #[test]
    fn review_notes_panel_add_is_reachable_by_tab_and_keyboard() {
        let mut agent = super::acceptance::Agent::new();
        let parent = agent.call("editor.spawn", json!({"name":"Target"}))["entity"]
            .as_u64()
            .expect("parent");
        let entity = crate::command::resolve_entity(agent.app.world(), parent).expect("entity");
        agent
            .app
            .world_mut()
            .resource_mut::<Selection>()
            .select(entity, SelectMode::Replace);
        agent.app.world_mut().insert_resource(NotesPanel {
            draft: "Keyboard review".into(),
            ..Default::default()
        });
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        crate::theme::sync(&ctx, agent.app.world());
        tab_to(&ctx, agent.app.world_mut(), "Add note");
        draw(&ctx, agent.app.world_mut(), vec![key(egui::Key::Space)], false);
        let notes = note_list(agent.app.world_mut(), &json!({})).expect("notes");
        assert_eq!(notes["notes"].as_array().expect("rows").len(), 1);
        assert_eq!(notes["notes"][0]["note"]["text"], "Keyboard review");
    }

    #[test]
    fn review_notes_resolve_does_not_reopen_during_second_ui_pass() {
        let mut agent = super::acceptance::Agent::new();
        let entity = agent.call(
            "editor.note_add",
            json!({"text":"Resolve once","position":[0,0,0]}),
        )["entity"]
            .as_u64()
            .expect("entity");
        agent.app.world_mut().insert_resource(NotesPanel {
            selected: Some(entity),
            ..Default::default()
        });
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        tab_to(&ctx, agent.app.world_mut(), "Resolve");
        let depth = agent.app.world().resource::<UndoStack>().undo_depth();
        draw(&ctx, agent.app.world_mut(), vec![key(egui::Key::Enter)], true);
        let notes = note_list(agent.app.world_mut(), &json!({})).expect("notes");
        assert_eq!(notes["notes"][0]["note"]["resolved"], true);
        assert_eq!(agent.app.world().resource::<UndoStack>().undo_depth(), depth + 1);
    }

    #[test]
    fn review_notes_viewport_badge_counts_thread_and_frames_from_keyboard() {
        let mut agent = super::acceptance::Agent::new();
        let visible = agent.call("editor.note_add", json!({"text":"Visible","position":[0,0,0]}))["entity"]
            .as_u64()
            .expect("marker");
        agent.call("editor.note_reply", json!({"entity":visible,"text":"Reply"}));
        agent.call(
            "editor.note_add",
            json!({"text":"Outside","position":[10000,0,0]}),
        );
        agent.call("editor.note_add", json!({"text":"Behind","position":[0,0,20]}));
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 100.0), egui::vec2(400.0, 300.0));
        let view = crate::views::ViewCamera {
            eye: Vec3::new(0.0, 0.0, 10.0),
            target: Vec3::ZERO,
            fov_y: 45.0_f32.to_radians(),
            near: 0.1,
            far: 1000.0,
            ortho: false,
        };
        let draw = |world: &mut World, events| {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 700.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| {
                    pins(ui, world, &view, rect, (0, 0));
                },
            );
            output.textures_delta.clear();
            output
        };
        let output = draw(
            agent.app.world_mut(),
            vec![egui::Event::PointerMoved(rect.center())],
        );
        let tree = output.platform_output.accesskit_update.expect("tree");
        assert!(crate::a11y_audit::unnamed_widgets(&tree).is_empty());
        let labels: Vec<_> = tree
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == egui::accesskit::Role::Button)
            .filter_map(|(_, node)| node.label())
            .collect();
        assert_eq!(
            labels,
            ["Note 2"],
            "only the visible marker draws, with its thread count"
        );
        assert!(agent.app.world().resource::<NotesPanel>().cursor_ray.is_some());
        draw(
            agent.app.world_mut(),
            vec![egui::Event::PointerMoved(egui::pos2(800.0, 600.0))],
        );
        assert!(agent.app.world().resource::<NotesPanel>().cursor_ray.is_none());
        draw(agent.app.world_mut(), vec![key(egui::Key::Tab)]);
        draw(agent.app.world_mut(), vec![key(egui::Key::Enter)]);
        assert_eq!(
            agent
                .app
                .world()
                .resource::<Selection>()
                .primary()
                .map(Entity::to_bits),
            Some(visible)
        );
    }
}
