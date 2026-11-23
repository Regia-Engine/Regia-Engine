// SPDX-License-Identifier: MPL-2.0
//! Session audit calls and undo edits in one monotonic timeline (ADR-7841).

use crate::command::{EditorCommand, UndoStack};
use crate::viewport_agent::Handler;
use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_agent::{AuditEntry, AuditLog, AuditObserver};
use regia_ecs::prelude::*;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

const CAP: usize = 2000;
static SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// One session activity item. Sequence ids are cursors, not wall-clock timestamps.
#[derive(Debug, Clone, Serialize)]
pub struct ActivityEntry {
    /// Monotonic process sequence, suitable for `editor.activity {since}`.
    pub seq: u64,
    /// Unix milliseconds for display; never used to order session entries.
    pub unix_ms: u64,
    /// `agent` or `user`.
    pub source: String,
    /// Agent connection name (`Agent` before hello), or `User`.
    pub who: String,
    /// RPC method or editor command description prefixed with edit/undo/redo.
    pub method: String,
    /// Touched entity ids, when known.
    pub entities: Vec<u64>,
    /// Touched asset paths, when known.
    pub assets: Vec<String>,
    /// `ok`, `error`, `expired`, `deferred`, or `board`.
    pub outcome: String,
}

impl ActivityEntry {
    fn new(source: &str, who: &str, method: String, unix_ms: u64) -> Self {
        Self {
            seq: SEQUENCE.fetch_add(1, Ordering::Relaxed),
            unix_ms,
            source: source.to_owned(),
            who: who.to_owned(),
            method,
            entities: Vec::new(),
            assets: Vec::new(),
            outcome: "ok".to_owned(),
        }
    }
}

/// UndoStack's bounded journal, separate from the coalescing undo entries.
#[derive(Default)]
pub(crate) struct CommandActivity {
    entries: VecDeque<ActivityEntry>,
    agent_call: bool,
    pending_entities: Vec<u64>,
    pending_assets: Vec<String>,
}

impl CommandActivity {
    #[tracing::instrument(skip_all)]
    pub(crate) fn record(&mut self, command: &dyn EditorCommand, operation: &str) {
        let mut entry = ActivityEntry::new(
            "user",
            "User",
            format!("{operation}: {}", command.description()),
            regia_agent::unix_ms_now(),
        );
        targets(command, &mut entry.entities, &mut entry.assets);
        entry.entities.sort_unstable();
        entry.entities.dedup();
        entry.assets.sort();
        entry.assets.dedup();
        if self.agent_call {
            self.pending_entities.extend(entry.entities);
            self.pending_assets.extend(entry.assets);
        } else {
            if self.entries.len() == CAP {
                self.entries.pop_front();
            }
            self.entries.push_back(entry);
        }
    }
}

fn targets(command: &dyn EditorCommand, entities: &mut Vec<u64>, assets: &mut Vec<String>) {
    use crate::command::*;
    let (extra_entities, extra_assets) = command.activity_targets();
    entities.extend(extra_entities);
    assets.extend(extra_assets);
    macro_rules! target {
        ($ty:ty) => {
            if let Some(command) = downcast_to::<$ty>(command) {
                entities.push(command.entity);
            }
        };
    }
    target!(SetProperty);
    target!(SetTransform);
    target!(Reparent);
    target!(AddComponent);
    target!(RemoveComponent);
    target!(SetPrefabOverrides);
    if let Some(command) = downcast_to::<CommandGroup>(command) {
        for child in &command.commands {
            targets(child.as_ref(), entities, assets);
        }
    }
    if let Some(command) = downcast_to::<crate::hierarchy_ops::CommandBatch>(command) {
        for child in &command.commands {
            targets(child.as_ref(), entities, assets);
        }
    }
    if let Some(command) = downcast_to::<crate::hierarchy_ops::ReparentEntities>(command) {
        entities.extend(command.moves.iter().map(|movement| movement.entity.bits));
    }
    if let Some(command) = downcast_to::<crate::viewport_ops::GizmoEdit>(command) {
        entities.extend(command.entries.iter().map(|entry| entry.0));
        if let Some(duplicate) = &command.duplicate {
            targets(duplicate.as_ref(), entities, assets);
        }
    }
    if let Some(command) = downcast_to::<TransformGesture>(command) {
        entities.push(command.0.entity);
    }
    if let Some(command) = downcast_to::<SpawnedRoots>(command) {
        entities.extend(&command.entities);
    }
    if let Some(command) = downcast_to::<DespawnEntities>(command) {
        entities.extend(&command.entities);
    }
    if let Some(command) = downcast_to::<SetProperty>(command)
        && let Some(path) = command
            .new
            .try_downcast_ref::<String>()
            .filter(|path| path.starts_with("assets/"))
    {
        assets.push(path.clone());
    }
}

#[derive(Resource, Default)]
struct ActivityPanel {
    history_loaded: bool,
    entries: VecDeque<ActivityEntry>,
    source: String,
    method: String,
    last_action: Option<u64>,
}

fn begin(world: &mut World) {
    let mut stack = world.get_resource_or_insert_with(UndoStack::new);
    stack.activity.agent_call = true;
    stack.activity.pending_entities.clear();
    stack.activity.pending_assets.clear();
}

fn recorded(world: &mut World, audit: &AuditEntry<'_>, params: &Value) {
    let who = if audit.client.is_empty() {
        "Agent"
    } else {
        audit.client
    };
    let mut entry = ActivityEntry::new("agent", who, audit.method.to_owned(), audit.unix_ms);
    entry.outcome = audit.outcome.to_owned();
    if let Some(mut stack) = world.get_resource_mut::<UndoStack>() {
        stack.activity.agent_call = false;
        entry.entities = std::mem::take(&mut stack.activity.pending_entities);
        entry.assets = std::mem::take(&mut stack.activity.pending_assets);
    }
    if let Some(entity) = params.get("entity").and_then(Value::as_u64) {
        entry.entities.push(entity);
    }
    if let Some(entities) = params.get("entities").and_then(Value::as_array) {
        // Refused requests are observed too; never retain an unbounded client-supplied list.
        entry
            .entities
            .extend(entities.iter().filter_map(Value::as_u64).take(256));
    }
    for key in ["asset", "path", "source", "destination"] {
        if let Some(path) = params
            .get(key)
            .and_then(Value::as_str)
            .filter(|path| path.starts_with("assets/") && path.len() <= 4096)
        {
            entry.assets.push(path.to_owned());
        }
    }
    // Successful spawn-style methods report new entity ids in the existing change log.
    if audit.outcome == "ok"
        && let Some(change) = world
            .get_resource::<regia_agent::ChangeLog>()
            .and_then(|log| log.records.last())
            .filter(|change| change.method == audit.method && change.frame == audit.frame)
        && let Some(entity) = change.summary.get("entity").and_then(Value::as_u64)
    {
        entry.entities.push(entity);
    }
    entry.entities.sort_unstable();
    entry.entities.dedup();
    entry.assets.sort();
    entry.assets.dedup();
    let mut panel = world.get_resource_or_insert_with(ActivityPanel::default);
    if panel.entries.len() == CAP {
        panel.entries.pop_front();
    }
    panel.entries.push_back(entry);
}

/// Installs the optional audit observer and reads the existing, bounded JSONL log once.
/// Historical audit rows did not store targets, so their target lists are empty.
pub fn install(world: &mut World) {
    world.insert_resource(AuditObserver {
        ready: load_history,
        begin,
        recorded,
    });
    world.get_resource_or_insert_with(ActivityPanel::default);
    load_history(world);
}

fn load_history(world: &mut World) {
    if world
        .get_resource::<ActivityPanel>()
        .is_some_and(|panel| panel.history_loaded)
    {
        return;
    }
    let Some(path) = world.get_resource::<AuditLog>().and_then(|log| log.path.clone()) else {
        return;
    };
    let mut panel = world.remove_resource::<ActivityPanel>().unwrap_or_default();
    panel.history_loaded = true;
    {
        use std::io::{BufRead, Read, Seek, SeekFrom};
        if let Ok(mut file) = std::fs::File::open(path) {
            let length = file.metadata().map_or(0, |meta| meta.len());
            let start = length.saturating_sub(1024 * 1024);
            if file.seek(SeekFrom::Start(start)).is_ok() {
                for (index, line) in std::io::BufReader::new(file.take(1024 * 1024))
                    .lines()
                    .enumerate()
                {
                    if index == 0 && start > 0 {
                        continue;
                    }
                    let line = match line {
                        Ok(line) => line,
                        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => continue,
                        Err(_) => break,
                    };
                    let Ok(row) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let Some(method) = row["method"].as_str() else {
                        continue;
                    };
                    let mut entry = ActivityEntry::new(
                        "agent",
                        row["client"]
                            .as_str()
                            .filter(|client| !client.is_empty())
                            .unwrap_or("Agent"),
                        method.to_owned(),
                        row["unix_ms"].as_u64().unwrap_or(0),
                    );
                    entry.outcome = row["outcome"].as_str().unwrap_or("unknown").to_owned();
                    if panel.entries.len() == CAP {
                        panel.entries.pop_front();
                    }
                    panel.entries.push_back(entry);
                }
            }
        }
    }
    world.insert_resource(panel);
}

/// Keeps session activity when a level/prefab switch replaces its undo stack.
pub(crate) fn retain_commands(world: &mut World) {
    let entries = world
        .get_resource_mut::<UndoStack>()
        .map(|mut stack| std::mem::take(&mut stack.activity.entries))
        .unwrap_or_default();
    if entries.is_empty() {
        return;
    }
    let mut panel = world.get_resource_or_insert_with(ActivityPanel::default);
    panel.entries.extend(entries);
    panel.entries.make_contiguous().sort_by_key(|entry| entry.seq);
    while panel.entries.len() > CAP {
        panel.entries.pop_front();
    }
}

fn activity(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let invalid = |message| {
        RpcError::new(
            INVALID_PARAMS,
            "activity_filter",
            message,
            "Use since: unsigned sequence; source: all/user/agent; method: substring",
        )
    };
    let since = match params.get("since") {
        None => 0,
        Some(value) => value.as_u64().ok_or_else(|| invalid("since must be unsigned"))?,
    };
    let source = match params.get("source") {
        None => "all",
        Some(value) => value
            .as_str()
            .filter(|source| ["all", "user", "agent"].contains(source))
            .ok_or_else(|| invalid("Unknown source"))?,
    };
    let method = match params.get("method") {
        None => "",
        Some(value) => value.as_str().ok_or_else(|| invalid("method must be text"))?,
    };
    let mut entries: Vec<_> = world
        .get_resource::<ActivityPanel>()
        .into_iter()
        .flat_map(|panel| panel.entries.iter())
        .chain(
            world
                .get_resource::<UndoStack>()
                .into_iter()
                .flat_map(|stack| stack.activity.entries.iter()),
        )
        .filter(|entry| {
            entry.seq > since && (source == "all" || source == entry.source) && entry.method.contains(method)
        })
        .cloned()
        .collect();
    entries.sort_by_key(|entry| entry.seq);
    let next = entries.last().map_or(since, |entry| entry.seq);
    Ok(json!({"entries": entries, "next": next}))
}

/// Activity protocol method and its typed parameter contract.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.activity",
            tool: "editor_activity",
            description: "Browse agent calls and this session's undo edits in sequence order.",
            params: &[
                ParamSchema {
                    name: "since",
                    kind: "number",
                    required: false,
                    description: "Exclusive sequence cursor; default 0.",
                },
                ParamSchema {
                    name: "source",
                    kind: "string",
                    required: false,
                    description: "all (default), user or agent.",
                },
                ParamSchema {
                    name: "method",
                    kind: "string",
                    required: false,
                    description: "Method/command substring filter.",
                },
            ],
            result: "{entries: [{seq, unix_ms, source, who, method, entities, assets, outcome}], next}",
            mutating: false,
            permission: Permission::Read,
        },
        activity as Handler,
    )]
}

/// An activity row's time (bug 21, ADR-8586): `HH:MM:SS` local wall time of `unix_ms` at
/// `offset_secs` east of UTC, and the full timestamp for the row's tooltip
/// (`2026-11-18 12:01:02.345 UTC-05:00`). The agent's `editor.activity` keeps `unix_ms`.
#[must_use]
pub fn wall_time(unix_ms: u64, offset_secs: i32) -> (String, String) {
    let ms = i64::try_from(unix_ms).unwrap_or(i64::MAX);
    let local = ms.div_euclid(1000) + i64::from(offset_secs);
    // `YYYY-MM-DDTHH:MM:SSZ` of the shifted instant: its fields are the local date and time.
    let stamp = crate::content::rfc3339_from_unix(local);
    let date = stamp.get(..10).unwrap_or("");
    let clock = stamp.get(11..19).unwrap_or("").to_owned();
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let off = offset_secs.unsigned_abs();
    let full = format!(
        "{date} {clock}.{:03} UTC{sign}{:02}:{:02}",
        ms.rem_euclid(1000),
        off / 3600,
        off % 3600 / 60
    );
    (clock, full)
}

/// Keyboard-accessible Activity tab with source/method filters and entity selection.
#[tracing::instrument(skip_all)]
pub fn panel(ui: &mut egui::Ui, world: &mut World) {
    let mut panel = world.remove_resource::<ActivityPanel>().unwrap_or_default();
    if panel.source.is_empty() {
        panel.source = "all".to_owned();
    }
    ui.horizontal(|ui| {
        ui.label("Source");
        for source in ["all", "agent", "user"] {
            ui.selectable_value(&mut panel.source, source.to_owned(), source);
        }
    });
    let label = ui.label("Method filter");
    ui.add(egui::TextEdit::singleline(&mut panel.method).hint_text("Method or command"))
        .labelled_by(label.id);
    let params = json!({"source": panel.source, "method": panel.method});
    world.insert_resource(panel);
    let entries = activity(world, &params).unwrap_or_else(|_| json!({"entries": []}));
    let frame = ui.ctx().cumulative_frame_nr();
    // Rows show local wall time; one offset per draw (ADR-8586).
    let now_secs = i64::try_from(regia_agent::unix_ms_now() / 1000).unwrap_or(0);
    let offset = regia_platform::local_utc_offset_seconds(now_secs).unwrap_or(0);
    egui::ScrollArea::vertical().show(ui, |ui| {
        for entry in entries["entries"].as_array().into_iter().flatten() {
            ui.push_id(entry["seq"].as_u64().unwrap_or(0), |ui| {
                let (clock, full) = wall_time(entry["unix_ms"].as_u64().unwrap_or(0), offset);
                ui.label(format!(
                    "{clock} · {} · {} · {}",
                    entry["who"].as_str().unwrap_or(""),
                    entry["method"].as_str().unwrap_or(""),
                    entry["outcome"].as_str().unwrap_or("")
                ))
                .on_hover_text(full);
                for bits in entry["entities"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_u64)
                {
                    let entity = crate::command::resolve_entity(world, bits);
                    if crate::agent_parity::action_enabled(
                        ui,
                        entity.is_some(),
                        &crate::agent_parity::ACTIVITY_SELECT,
                        Some(format!("Select entity {bits}")),
                    )
                    .clicked()
                        && world.resource::<ActivityPanel>().last_action != Some(frame)
                        && let Some(entity) = entity
                    {
                        world
                            .get_resource_or_insert_with(crate::selection::Selection::default)
                            .select(entity, crate::selection::SelectMode::Replace);
                        world.resource_mut::<ActivityPanel>().last_action = Some(frame);
                    }
                }
                for asset in entry["assets"].as_array().into_iter().flatten() {
                    ui.label(asset.as_str().unwrap_or(""));
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use regia_app::prelude::*;
    use serde_json::json;

    /// Bug 21 (ADR-8586): a row shows `HH:MM:SS` local time, the tooltip the full timestamp.
    #[test]
    fn activity_rows_show_wall_time_not_epoch_milliseconds() {
        // 1794880193750 ms = 2026-11-17 01:49:53.750 UTC (the smoke run's row).
        assert_eq!(
            super::wall_time(1_794_880_193_750, 0),
            (
                "01:49:53".to_owned(),
                "2026-11-17 01:49:53.750 UTC+00:00".to_owned()
            )
        );
        assert_eq!(
            super::wall_time(1_794_880_193_750, -5 * 3600),
            (
                "20:49:53".to_owned(),
                "2026-11-16 20:49:53.750 UTC-05:00".to_owned()
            )
        );
        assert_eq!(super::wall_time(1_794_880_193_750, 5 * 3600 + 1800).0, "07:19:53");
        assert_eq!(super::wall_time(0, 0).1, "1970-01-01 00:00:00.000 UTC+00:00");
    }

    #[test]
    fn activity_panel_agent_contract_exists() {
        let mut world = World::new();
        crate::agent_ext::install(&mut world);
        let methods = crate::agent_ext::all_methods();
        let (_, activity) = methods
            .iter()
            .find(|(schema, _)| schema.name == "editor.activity")
            .expect("missing editor.activity");
        assert_eq!(
            activity(&mut world, &json!({"since": 0})).expect("activity")["entries"],
            json!([])
        );
    }

    #[test]
    fn activity_panel_reads_existing_audit_and_skips_malformed_rows() {
        let root = regia_test_utils::unique_temp_dir("review-activity-log");
        std::fs::create_dir_all(&root).expect("directory");
        let path = root.join("audit.jsonl");
        let mut bytes =
            br#"{"method":"editor.spawn","client":"Earlier connection","unix_ms":1,"outcome":"ok"}
not json
"#
            .to_vec();
        bytes.extend_from_slice(&[0xff, b'\n']);
        bytes.extend_from_slice(
            br#"{"method":"editor.set_property","client":"","unix_ms":2,"outcome":"ok"}
"#,
        );
        std::fs::write(&path, bytes).expect("audit fixture");
        let mut world = World::new();
        world.insert_resource(regia_agent::AuditLog::open(path));
        super::install(&mut world);
        let rows = super::activity(&mut world, &json!({})).expect("history");
        assert_eq!(rows["entries"].as_array().expect("rows").len(), 2);
        assert_eq!(rows["entries"][0]["who"], "Earlier connection");
        assert_eq!(rows["entries"][1]["who"], "Agent");
        assert_eq!(rows["entries"][1]["method"], "editor.set_property");
        assert_eq!(rows["entries"][1]["entities"], json!([]));
        drop(world);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn activity_panel_loads_audit_after_agent_plugin_finishes() {
        let mut agent = crate::review_notes::acceptance::Agent::with_history(
            "{\"method\":\"editor.spawn\",\"client\":\"Previous session\",\"unix_ms\":1,\"outcome\":\"ok\"}\n",
        );
        let rows = agent.call("editor.activity", json!({}));
        assert_eq!(rows["entries"][0]["who"], "Previous session");
        super::install(agent.app.world_mut());
        let rows = agent.call("editor.activity", json!({"method":"editor.spawn"}));
        assert_eq!(
            rows["entries"].as_array().expect("rows").len(),
            1,
            "history loads once"
        );
    }

    #[test]
    fn activity_panel_cursor_filters_and_level_switch_keep_session_edits() {
        use crate::command::{EditorCommand, SetTransform, UndoStack};
        let mut agent = crate::review_notes::acceptance::Agent::new();
        let bits = agent.call("editor.spawn", json!({"name":"Target"}))["entity"]
            .as_u64()
            .expect("entity");
        let world = agent.app.world_mut();
        let entity = crate::command::resolve_entity(world, bits).expect("entity");
        let mut edit = SetTransform::capture(world, entity, Default::default());
        edit.apply(world);
        world.resource_mut::<UndoStack>().push(Box::new(edit));
        let before = super::activity(world, &json!({"source":"user"})).expect("history");
        assert_eq!(before["entries"].as_array().expect("rows").len(), 1);
        super::retain_commands(world);
        world.insert_resource(UndoStack::new());
        assert_eq!(
            super::activity(world, &json!({"source":"user"})).expect("retained"),
            before
        );
        assert_eq!(
            super::activity(world, &json!({"since":before["next"],"source":"user"})).expect("cursor")["entries"],
            json!([])
        );
        for params in [
            json!({"since":-1}),
            json!({"source":"invalid"}),
            json!({"method":false}),
        ] {
            let error = agent
                .try_call("editor.activity", params)
                .expect_err("invalid filter");
            assert_eq!(error.data.expect("error data")["code"], "activity_filter");
        }
    }

    #[test]
    fn activity_panel_user_spawn_records_targets_through_undo_and_redo() {
        use crate::command::{EditorCommand, SpawnEntities, UndoStack};
        let mut agent = crate::review_notes::acceptance::Agent::new();
        let world = agent.app.world_mut();
        let mut command = SpawnEntities::new_entity(
            world,
            regia_core::id::Guid::from_u128(42),
            "User item",
            Default::default(),
        )
        .expect("command");
        command.apply(world);
        let entity = world
            .query::<(Entity, &regia_scene::prelude::EntityGuid)>()
            .iter(world)
            .find_map(|(entity, guid)| (guid.0 == regia_core::id::Guid::from_u128(42)).then_some(entity))
            .expect("created");
        world.resource_mut::<UndoStack>().push(Box::new(command));
        let rows = super::activity(world, &json!({"source":"user"})).expect("history");
        assert_eq!(rows["entries"][0]["entities"], json!([entity.to_bits()]));
        assert!(crate::command::undo_world(world));
        assert!(crate::command::resolve_entity(world, entity.to_bits()).is_none());
        assert!(crate::command::redo_world(world));
        assert!(crate::command::resolve_entity(world, entity.to_bits()).is_some());
        let rows = super::activity(world, &json!({"source":"user"})).expect("history");
        assert_eq!(rows["entries"].as_array().expect("rows").len(), 3);
        assert_eq!(rows["entries"][2]["entities"], json!([entity.to_bits()]));
    }
}

#[cfg(test)]
mod keyboard_tests {
    use super::*;
    use crate::selection::Selection;

    #[test]
    fn activity_panel_target_selection_is_keyboard_operable() {
        let mut agent = crate::review_notes::acceptance::Agent::new();
        let bits = agent.call("editor.spawn", json!({"name":"Activity target"}))["entity"]
            .as_u64()
            .expect("entity");
        let target = crate::command::resolve_entity(agent.app.world(), bits).expect("target");
        agent.app.world_mut().resource_mut::<Selection>().clear();
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
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
                    egui::CentralPanel::default().show(ui, |ui| panel(ui, world));
                },
            );
            output.textures_delta.clear();
            let update = output.platform_output.accesskit_update.expect("accessible tree");
            assert!(crate::a11y_audit::unnamed_widgets(&update).is_empty());
            update
                .nodes
                .iter()
                .find(|(id, _)| *id == update.focus)
                .and_then(|(_, node)| node.label())
                .unwrap_or_default()
                .to_owned()
        };
        let key = |key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        draw(agent.app.world_mut(), vec![]);
        let mut reached = false;
        for _ in 0..14 {
            let focused = draw(agent.app.world_mut(), vec![key(egui::Key::Tab)]);
            if focused == format!("Select entity {bits}") {
                reached = true;
                draw(agent.app.world_mut(), vec![key(egui::Key::Enter)]);
                break;
            }
        }
        assert!(reached, "Tab reaches the named target button");
        assert_eq!(agent.app.world().resource::<Selection>().primary(), Some(target));
    }

    #[test]
    fn activity_panel_same_clock_tick_keeps_call_and_edit_order() {
        use crate::command::{EditorCommand, SetTransform};
        let mut agent = crate::review_notes::acceptance::Agent::new();
        let bits = agent.call("editor.spawn", json!({"name":"Target"}))["entity"]
            .as_u64()
            .expect("entity");
        let world = agent.app.world_mut();
        let entity = crate::command::resolve_entity(world, bits).expect("entity");
        let mut edit = SetTransform::capture(world, entity, Default::default());
        edit.apply(world);
        world.resource_mut::<UndoStack>().push(Box::new(edit));
        // Force all displayed clocks equal; sequence order must be unchanged.
        for entry in &mut world.resource_mut::<ActivityPanel>().entries {
            entry.unix_ms = 42;
        }
        for entry in &mut world.resource_mut::<UndoStack>().activity.entries {
            entry.unix_ms = 42;
        }
        let rows = activity(world, &json!({})).expect("activity");
        let entries = rows["entries"].as_array().expect("rows");
        assert_eq!(entries[entries.len() - 2]["source"], "agent");
        assert_eq!(entries[entries.len() - 1]["source"], "user");
    }
}
