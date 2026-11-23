// SPDX-License-Identifier: MPL-2.0
//! Engine-message tap (specs/23 § 3.7): records the message kinds the protocol can poll into a
//! small ring log so agents can follow what happened — routed sim events, UI toasts and live
//! settings changes. Log records are read through `events.read` (cursor pattern like
//! [`crate::dispatch`] handles `logs.read`); `events.list` reports what is tapped.
//!
//! The tap is deliberately small and typed: these three kinds are the ones the runtime surface
//! itself reacts to. Arbitrary project messages would need reflection-based message
//! registration, which arrives with the editor surface.

use regia_app::prelude::*;
use regia_ecs::ecs::message::MessageCursor;
use serde_json::{Value, json};

use crate::protocol::RpcError;
use crate::safety::unix_ms_now;

/// How many records the tap keeps (older ones drop off).
const CAPACITY: usize = 512;

/// One recorded engine message.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentEventRecord {
    /// Sequence number (the `since` cursor of `events.read`).
    pub seq: u64,
    /// Frame the message was seen in.
    pub frame: u64,
    /// Unix ms the message was seen at.
    pub unix_ms: u64,
    /// Tapped kind: `sim_event`, `toast`, `settings`, `extension` or `spacemouse`.
    pub kind: &'static str,
    /// What happened, as JSON.
    pub data: Value,
}

/// The tapped engine messages of this session (`events.read`).
#[derive(Resource, Default)]
pub struct AgentEventLog {
    pub(crate) records: Vec<AgentEventRecord>,
    seq: u64,
    cursor_routed: MessageCursor<regia_framework::sim_bridge::RoutedEvent>,
    cursor_toast: MessageCursor<regia_framework::sim_bridge::ToastRaised>,
    cursor_settings: MessageCursor<regia_framework::settings::SettingsChanged>,
    cursor_extension: MessageCursor<regia_framework::extension::ExtensionEvent>,
    procgen: ProcgenCursors,
    #[cfg(not(target_os = "android"))]
    spacemouse: SpaceMouseCursors,
}

/// Cursors over the procedural-generation lifecycle messages plus the resident-cell sets seen last
/// frame (`procgen.cells_changed` is the difference).
#[derive(Default)]
struct ProcgenCursors {
    started: MessageCursor<regia_procgen::world::ProcGenStarted>,
    finished: MessageCursor<regia_procgen::world::ProcGenFinished>,
    failed: MessageCursor<regia_procgen::world::ProcGenFailed>,
    cancelled: MessageCursor<regia_procgen::world::ProcGenCancelled>,
    cleared: MessageCursor<regia_procgen::world::ProcGenCleared>,
    detached: MessageCursor<regia_procgen::world::ProcGenDetached>,
    resident: std::collections::BTreeMap<Entity, std::collections::BTreeSet<regia_procgen::tiles::TileKey>>,
}

/// Cursors over the SpaceMouse messages (desktop; specs/29 § 2.5: button actions are agent events).
#[cfg(not(target_os = "android"))]
#[derive(Default)]
struct SpaceMouseCursors {
    actions: MessageCursor<regia_input::spacemouse::SpaceMouseAction>,
    connected: MessageCursor<regia_input::spacemouse::SpaceMouseConnected>,
    disconnected: MessageCursor<regia_input::spacemouse::SpaceMouseDisconnected>,
}

/// Records the SpaceMouse button actions and hot-plug messages of this frame as `spacemouse`.
#[cfg(not(target_os = "android"))]
fn record_spacemouse(world: &World, log: &mut AgentEventLog, frame: u64) {
    use regia_input::spacemouse::{SpaceMouseAction, SpaceMouseConnected, SpaceMouseDisconnected};
    let mut records = Vec::new();
    if let Some(messages) = world.get_resource::<Messages<SpaceMouseConnected>>() {
        for m in log.spacemouse.connected.read(messages) {
            records.push(json!({ "event": "connected", "id": m.device.id, "name": m.device.name }));
        }
    }
    if let Some(messages) = world.get_resource::<Messages<SpaceMouseDisconnected>>() {
        for m in log.spacemouse.disconnected.read(messages) {
            records.push(json!({ "event": "disconnected", "id": m.id }));
        }
    }
    if let Some(messages) = world.get_resource::<Messages<SpaceMouseAction>>() {
        for m in log.spacemouse.actions.read(messages) {
            records.push(json!({ "event": "action", "action": m.action, "button": m.button }));
        }
    }
    for record in records {
        log.push(frame, "spacemouse", record);
    }
}

impl AgentEventLog {
    pub(crate) fn push(&mut self, frame: u64, kind: &'static str, data: Value) {
        self.seq += 1;
        self.records.push(AgentEventRecord {
            seq: self.seq,
            frame,
            unix_ms: unix_ms_now(),
            kind,
            data,
        });
        if self.records.len() > CAPACITY {
            self.records.drain(..self.records.len() - CAPACITY);
        }
    }

    /// Records a host-defined kind (the editor's `tool_progress`, ADR-1523) at `frame`, read back
    /// through `events.read` like the tapped kinds.
    pub fn push_host(&mut self, frame: u64, kind: &'static str, data: Value) {
        self.push(frame, kind, data);
    }

    /// The newest `limit` records, oldest first (read back through `events.read`).
    pub fn records_snapshot(&self, limit: usize) -> Vec<AgentEventRecord> {
        let start = self.records.len().saturating_sub(limit);
        self.records[start..].to_vec()
    }
}

/// `Last`: drains the tapped message buffers into the log. Runs against the whole world so the
/// cursors can live in the log resource (message buffers exist only when their plugin does).
pub fn record_events(world: &mut World) {
    let frame = world.resource::<Time>().frame_count();
    let Some(mut log) = world.remove_resource::<AgentEventLog>() else {
        return;
    };
    if let Some(messages) = world.get_resource::<Messages<regia_framework::sim_bridge::RoutedEvent>>() {
        let events: Vec<_> = log.cursor_routed.read(messages).cloned().collect();
        for event in events {
            let reactions = serde_json::to_value(&event.reactions).unwrap_or_else(|_| json!([]));
            log.push(
                frame,
                "sim_event",
                json!({ "kind": event.kind, "reactions": reactions }),
            );
        }
    }
    if let Some(messages) = world.get_resource::<Messages<regia_framework::sim_bridge::ToastRaised>>() {
        let toasts: Vec<_> = log.cursor_toast.read(messages).cloned().collect();
        for toast in toasts {
            log.push(frame, "toast", json!({ "text": toast.text }));
        }
    }
    if let Some(messages) = world.get_resource::<Messages<regia_framework::settings::SettingsChanged>>() {
        let registry = world.resource::<AppTypeRegistry>().clone();
        let registry = registry.read();
        let changes: Vec<_> = log.cursor_settings.read(messages).cloned().collect();
        for change in changes {
            log.push(
                frame,
                "settings",
                json!({
                    "field": changed_field(&change.before, &change.after, &registry),
                    "before": crate::scene::serialize_reflect(
                        change.before.as_partial_reflect(),
                        &registry,
                    )
                    .unwrap_or(Value::Null),
                    "after": crate::scene::serialize_reflect(
                        change.after.as_partial_reflect(),
                        &registry,
                    )
                    .unwrap_or(Value::Null),
                }),
            );
        }
    }
    if let Some(messages) = world.get_resource::<Messages<regia_framework::extension::ExtensionEvent>>() {
        let events: Vec<_> = log.cursor_extension.read(messages).cloned().collect();
        for event in events {
            log.push(
                frame,
                "extension",
                serde_json::to_value(&event).unwrap_or(Value::Null),
            );
        }
    }
    #[cfg(not(target_os = "android"))]
    record_spacemouse(world, &mut log, frame);
    record_procgen(world, &mut log, frame);
    world.insert_resource(log);
}

/// The generator's guid as text (`""` when the entity has none).
fn generator_guid(world: &World, entity: Entity) -> String {
    world
        .get::<regia_scene::components::EntityGuid>(entity)
        .map(|g| g.0.to_string())
        .unwrap_or_default()
}

/// A tile key as `[tier, i, j]`.
fn key_json(k: &regia_procgen::tiles::TileKey) -> Value {
    json!([k.tier, k.cell.x, k.cell.z])
}

/// Records this frame's `procgen.*` lifecycle events (specs/62 § 7.2). Within a frame the events of
/// one generator are ordered by run id, then started, cancelled/finished/failed; `cleared` and
/// `detached` come last. Different generators are ordered by entity.
fn record_procgen(world: &World, log: &mut AgentEventLog, frame: u64) {
    use regia_procgen::world::{
        ProcGenCancelled, ProcGenCleared, ProcGenDetached, ProcGenFailed, ProcGenFinished, ProcGenStarted,
    };
    // (entity, run key, rank, kind, data)
    let mut batch: Vec<(Entity, u64, u8, &'static str, Value)> = Vec::new();
    let head = |world: &World, entity: Entity, run_id: u64| json!({ "generator": generator_guid(world, entity), "entity": entity.to_bits(), "run_id": run_id });
    // `cleared` and `detached` are not run events (events.list documents them without a run_id);
    // they carry only the generator and entity (detached adds `new_root`).
    let bare = |world: &World, entity: Entity| json!({ "generator": generator_guid(world, entity), "entity": entity.to_bits() });
    let with = |mut base: Value, extra: Value| {
        if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        base
    };
    if let Some(m) = world.get_resource::<Messages<ProcGenStarted>>() {
        for e in log.procgen.started.read(m) {
            batch.push((
                e.entity,
                e.run_id,
                1,
                "procgen.run_started",
                head(world, e.entity, e.run_id),
            ));
        }
    }
    if let Some(m) = world.get_resource::<Messages<ProcGenCancelled>>() {
        for e in log.procgen.cancelled.read(m) {
            batch.push((
                e.entity,
                e.run_id,
                2,
                "procgen.run_cancelled",
                head(world, e.entity, e.run_id),
            ));
        }
    }
    if let Some(m) = world.get_resource::<Messages<ProcGenFinished>>() {
        for e in log.procgen.finished.read(m) {
            let hash: String = e.output_hash.iter().map(|b| format!("{b:02x}")).collect();
            let extra = json!({
                "ok": true,
                "output_hash": hash,
                "counts": { "instances": e.counts[0], "entities": e.counts[1], "tables": e.counts[2] },
            });
            batch.push((
                e.entity,
                e.run_id,
                3,
                "procgen.run_finished",
                with(head(world, e.entity, e.run_id), extra),
            ));
        }
    }
    if let Some(m) = world.get_resource::<Messages<ProcGenFailed>>() {
        for e in log.procgen.failed.read(m) {
            let extra = json!({ "ok": false, "code": e.code, "node": e.node });
            batch.push((
                e.entity,
                e.run_id,
                3,
                "procgen.run_finished",
                with(head(world, e.entity, e.run_id), extra),
            ));
        }
    }
    if let Some(m) = world.get_resource::<Messages<ProcGenCleared>>() {
        for e in log.procgen.cleared.read(m) {
            batch.push((e.entity, u64::MAX, 4, "procgen.cleared", bare(world, e.entity)));
        }
    }
    if let Some(m) = world.get_resource::<Messages<ProcGenDetached>>() {
        for e in log.procgen.detached.read(m) {
            let extra = json!({ "new_root": e.new_root.to_bits() });
            batch.push((
                e.entity,
                u64::MAX,
                5,
                "procgen.detached",
                with(bare(world, e.entity), extra),
            ));
        }
    }
    batch.sort_by_key(|b| (b.0, b.1, b.2));
    for (_, _, _, kind, data) in batch {
        log.push(frame, kind, data);
    }
    // Resident-cell changes of tiled generators.
    if let Some(tiles) = world.get_resource::<regia_procgen::tiles::ProcGenTiles>() {
        use regia_procgen::tiles::TileState;
        let _span = tracing::info_span!("procgen.event_tap_cells").entered();
        let mut now = std::collections::BTreeMap::new();
        for (entity, g) in &tiles.generators {
            let set: std::collections::BTreeSet<_> = g
                .entries
                .iter()
                .filter(|(_, e)| e.state == TileState::Resident)
                .map(|(k, _)| *k)
                .collect();
            now.insert(*entity, set);
        }
        let mut changes = Vec::new();
        for (entity, set) in &now {
            let before = log.procgen.resident.get(entity);
            let added: Vec<Value> = set
                .iter()
                .filter(|k| before.is_none_or(|b| !b.contains(k)))
                .map(key_json)
                .collect();
            let released: Vec<Value> = before
                .into_iter()
                .flatten()
                .filter(|k| !set.contains(k))
                .map(key_json)
                .collect();
            if !added.is_empty() || !released.is_empty() {
                changes.push(json!({
                    "generator": generator_guid(world, *entity),
                    "entity": entity.to_bits(),
                    "added": added,
                    "released": released,
                    "resident": set.len(),
                }));
            }
        }
        for (entity, before) in &log.procgen.resident {
            if !now.contains_key(entity) && !before.is_empty() {
                changes.push(json!({
                    "generator": generator_guid(world, *entity),
                    "entity": entity.to_bits(),
                    "added": [],
                    "released": before.iter().map(key_json).collect::<Vec<_>>(),
                    "resident": 0,
                }));
            }
        }
        log.procgen.resident = now;
        for data in changes {
            log.push(frame, "procgen.cells_changed", data);
        }
    }
}

/// Records `procgen.bake_written` for a bake the protocol just wrote (`paths` are the files).
pub(crate) fn push_bake_written(world: &mut World, entity: Entity, paths: &[String], cells: Option<usize>) {
    let frame = world.get_resource::<Time>().map_or(0, |t| t.frame_count());
    let data = json!({
        "generator": generator_guid(world, entity),
        "entity": entity.to_bits(),
        "files": paths,
        "cells": cells,
    });
    if let Some(mut log) = world.get_resource_mut::<AgentEventLog>() {
        log.push(frame, "procgen.bake_written", data);
    }
}

/// The first top-level field that differs between two settings values ("" when unserialisable).
fn changed_field(
    before: &regia_framework::settings::GameSettings,
    after: &regia_framework::settings::GameSettings,
    registry: &regia_ecs::reflect::TypeRegistry,
) -> String {
    let Ok(before) = crate::scene::serialize_reflect(before.as_partial_reflect(), registry) else {
        return String::new();
    };
    let Ok(after) = crate::scene::serialize_reflect(after.as_partial_reflect(), registry) else {
        return String::new();
    };
    if let (Some(before), Some(after)) = (before.as_object(), after.as_object()) {
        for (key, new) in after {
            if before.get(key) != Some(new) {
                return key.clone();
            }
        }
    }
    String::new()
}

/// The tapped kinds, with a line each for `events.list`.
fn tapped_kinds() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "sim_event",
            "Routed sim events (regia_framework::sim_bridge::RoutedEvent): the event kind and its mapped reactions.",
        ),
        (
            "toast",
            "UI toasts raised by the event router (sim_bridge::ToastRaised).",
        ),
        (
            "settings",
            "Live GameSettings changes (settings::SettingsChanged): the changed field, before and after.",
        ),
        (
            "extension",
            "Extension life cycle (extension::ExtensionEvent): {Built: {id, phase}}, {Failed: {id, code}}, Resolved, Enabled, Disabled, Reloaded, PackActivated, PackDeactivated.",
        ),
        (
            "spacemouse",
            "SpaceMouse button actions ({event: action, action, button}) and hot-plug ({event: connected|disconnected, id}); desktop.",
        ),
        (
            "procgen.run_started",
            "A procedural generator run started ({generator, entity, run_id}).",
        ),
        (
            "procgen.run_finished",
            "A run ended ({generator, entity, run_id, ok}); ok adds output_hash and counts {instances, entities, tables}, a failure adds code and node.",
        ),
        (
            "procgen.run_cancelled",
            "A run was cancelled by a newer request or a clear ({generator, entity, run_id}).",
        ),
        (
            "procgen.bake_written",
            "procgen.bake wrote a bake ({generator, entity, files, cells}).",
        ),
        (
            "procgen.cells_changed",
            "Resident cells of a tiled generator changed ({generator, entity, added, released, resident}; cells are [tier, i, j]).",
        ),
        (
            "procgen.cleared",
            "A generator's output was removed ({generator, entity}).",
        ),
        (
            "procgen.detached",
            "A generator's output became level content ({generator, entity, new_root}).",
        ),
    ]
}

/// `events.list`.
pub(crate) fn list() -> Value {
    let events: Vec<Value> = tapped_kinds()
        .into_iter()
        .map(|(kind, description)| json!({ "kind": kind, "description": description }))
        .collect();
    json!({ "events": events })
}

/// `events.read`: the oldest `limit` records newer than `since` (oldest first), and the cursor
/// for the next call (the last returned `seq`, or `since` when nothing was returned), so paging
/// with a small `limit` never skips records.
pub(crate) fn read(world: &World, params: &Value) -> Result<Value, RpcError> {
    let since = crate::params::opt_u64(params, "since")?.unwrap_or(0);
    let limit =
        crate::params::opt_u64(params, "limit")?.map_or(200, |l| usize::try_from(l).unwrap_or(usize::MAX));
    let records: Vec<&AgentEventRecord> = world
        .get_resource::<AgentEventLog>()
        .map(|log| {
            log.records
                .iter()
                .filter(|record| record.seq > since)
                .take(limit)
                .collect()
        })
        .unwrap_or_default();
    let next = records.last().map_or(since, |record| record.seq);
    let records: Vec<Value> = records
        .into_iter()
        .map(|record| {
            json!({
                "seq": record.seq,
                "frame": record.frame,
                "unix_ms": record.unix_ms,
                "kind": record.kind,
                "data": record.data,
            })
        })
        .collect();
    Ok(json!({ "records": records, "next": next }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::settings::{GameSettings, SettingsPath, SettingsPlugin};
    use regia_framework::sim_bridge::ToastRaised;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(SettingsPlugin)
            .insert_resource(SettingsPath(None))
            .init_resource::<AgentEventLog>()
            .add_message::<ToastRaised>()
            .add_message::<regia_framework::sim_bridge::RoutedEvent>()
            .add_message::<regia_framework::settings::SettingsChanged>()
            .add_systems(Last, record_events)
            .insert_resource(TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_millis(16),
            ));
        app
    }

    #[test]
    fn lists_the_tapped_kinds() {
        let listed = list();
        let kinds: Vec<&str> = listed["events"]
            .as_array()
            .map(|a| a.iter().filter_map(|e| e["kind"].as_str()).collect())
            .unwrap_or_default();
        assert_eq!(
            kinds,
            [
                "sim_event",
                "toast",
                "settings",
                "extension",
                "spacemouse",
                "procgen.run_started",
                "procgen.run_finished",
                "procgen.run_cancelled",
                "procgen.bake_written",
                "procgen.cells_changed",
                "procgen.cleared",
                "procgen.detached",
            ]
        );
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn records_spacemouse_actions_and_hot_plug() {
        use regia_input::spacemouse::{FakeSpaceMouse, SpaceMousePlugin, SpaceMouseState};
        let fake = FakeSpaceMouse::default();
        let mut app = app();
        app.add_plugins(SpaceMousePlugin::with_backend(fake.backend()))
            .add_plugins(regia_input::InputPlugin::default());
        fake.connect(4, "SpaceMouse Pro");
        fake.sample(
            4,
            SpaceMouseState {
                buttons: 0b1,
                ..Default::default()
            },
        );
        app.update();
        fake.disconnect(4);
        app.update();
        let out = read(app.world(), &json!({})).expect("reads");
        let events: Vec<&Value> = out["records"]
            .as_array()
            .map(|r| {
                r.iter()
                    .filter(|r| r["kind"] == "spacemouse")
                    .map(|r| &r["data"])
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(events.len(), 3, "{out}");
        assert_eq!(events[0]["event"], "connected");
        assert_eq!(events[1]["action"], "view.fit_selection");
        assert_eq!(events[2]["event"], "disconnected");
    }

    #[test]
    fn reads_toasts_and_settings_changes_with_cursors() {
        let mut app = app();
        app.update();
        {
            let world = app.world_mut();
            world.write_message(ToastRaised {
                text: "Sim online".into(),
            });
            let mut settings = world.resource_mut::<GameSettings>();
            settings.input.sensitivity = 3.0;
        }
        app.update();
        app.update(); // the settings plugin announces the change

        let first = read(app.world(), &json!({})).expect("reads");
        let records = first["records"].as_array().expect("records");
        assert!(records.len() >= 2, "toast + settings: {first}");
        let toast = records.iter().find(|r| r["kind"] == "toast").expect("toast");
        assert_eq!(toast["data"]["text"], "Sim online");
        let settings = records
            .iter()
            .find(|r| r["kind"] == "settings")
            .expect("settings change");
        assert_eq!(settings["data"]["field"], "input");
        assert_eq!(settings["data"]["after"]["input"]["sensitivity"], 3.0);
        let next = first["next"].as_u64().expect("cursor");

        // since = next → nothing new.
        let empty = read(app.world(), &json!({ "since": next })).expect("reads");
        assert_eq!(empty["records"].as_array().map(Vec::len), Some(0));
    }

    fn seqs(out: &Value) -> Vec<u64> {
        out["records"]
            .as_array()
            .map(|r| r.iter().filter_map(|r| r["seq"].as_u64()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn events_read_pages_oldest_unread_without_skipping() {
        let mut app = app();
        for i in 1..=5 {
            app.world_mut()
                .resource_mut::<AgentEventLog>()
                .push(0, "toast", json!({ "i": i }));
        }
        let page = read(app.world(), &json!({ "since": 0, "limit": 2 })).expect("reads");
        assert_eq!(seqs(&page), [1, 2]);
        assert_eq!(page["next"], 2);
        let page = read(app.world(), &json!({ "since": 2, "limit": 2 })).expect("reads");
        assert_eq!(seqs(&page), [3, 4]);
        assert_eq!(page["next"], 4);
        let page = read(app.world(), &json!({ "since": 4, "limit": 2 })).expect("reads");
        assert_eq!(seqs(&page), [5]);
        assert_eq!(page["next"], 5);
        let empty = read(app.world(), &json!({ "since": 5 })).expect("reads");
        assert!(seqs(&empty).is_empty());
        assert_eq!(empty["next"], 5, "an exhausted cursor stays put");
        let zero = read(app.world(), &json!({ "since": 0, "limit": 0 })).expect("reads");
        assert!(seqs(&zero).is_empty());
        assert_eq!(zero["next"], 0);
    }

    #[test]
    fn events_contain_matches_a_record_older_than_the_newest_200() {
        let mut app = app();
        {
            let mut log = app.world_mut().resource_mut::<AgentEventLog>();
            log.push(0, "toast", json!({ "text": "NEEDLE" }));
            for i in 0..250 {
                log.push(0, "toast", json!({ "text": format!("filler {i}") }));
            }
        }
        let finding = crate::playtest::evaluate(app.world_mut(), &json!({ "events_contain": "NEEDLE" }));
        assert!(finding.holds, "{finding:?}");
    }

    #[test]
    fn events_read_pages_two_at_a_time_from_one_writer() {
        let mut app = app();
        {
            let mut log = app.world_mut().resource_mut::<AgentEventLog>();
            for i in 0..5 {
                log.push(1, "toast", json!({ "i": i }));
            }
        }
        let seqs = |v: &Value| -> Vec<u64> {
            v["records"]
                .as_array()
                .expect("records")
                .iter()
                .map(|r| r["seq"].as_u64().expect("seq"))
                .collect()
        };
        let a = read(app.world(), &json!({ "since": 0, "limit": 2 })).expect("reads");
        assert_eq!(seqs(&a), vec![1, 2]);
        assert_eq!(a["next"], 2);
        let b = read(app.world(), &json!({ "since": 2, "limit": 2 })).expect("reads");
        assert_eq!(seqs(&b), vec![3, 4]);
        assert_eq!(b["next"], 4);
        let z = read(app.world(), &json!({ "since": 0, "limit": 0 })).expect("reads");
        assert!(seqs(&z).is_empty());
        assert_eq!(z["next"], 0);
    }

    #[test]
    fn log_keeps_only_the_newest_records() {
        let mut log = AgentEventLog::default();
        for i in 0..(CAPACITY + 40) {
            log.push(i as u64, "toast", json!({ "i": i }));
        }
        assert_eq!(log.records.len(), CAPACITY);
        let snapshot = log.records_snapshot(3);
        assert_eq!(snapshot.len(), 3);
        assert_eq!(snapshot[0].data["i"], CAPACITY + 39 - 2, "newest kept");
        assert_eq!(snapshot[0].seq as usize, CAPACITY + 40 - 2);
    }

    /// Spec 62 § 7.2: a run emits `procgen.run_started` then `procgen.run_finished` with the same run
    /// id and the generator guid; a failing graph finishes with `ok: false` and its error code.
    /// (`procgen.bake_written`, `procgen.run_cancelled`, `procgen.cells_changed`, `procgen.cleared`
    /// and `procgen.detached` have their own tests below.)
    #[test]
    fn procgen_lifecycle_reaches_the_event_stream() {
        use regia_procgen::world::{ProcGenPlugin, ProcGenSettings, ProcGenerator, ProcGraphSource};
        use regia_scene::components::EntityGuid;
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(ProcGenSettings {
                inline: true,
                source: ProcGraphSource::Memory(std::collections::BTreeMap::new()),
                ..ProcGenSettings::default()
            })
            .add_plugins(ProcGenPlugin)
            .init_resource::<AgentEventLog>()
            .add_systems(Last, record_events)
            .insert_resource(TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_millis(16),
            ));
        let w = app.world_mut();
        crate::procgen::graph_edit(
            w,
            &json!({ "path": "g.pgraph.ron", "ops": [
                { "op": "add_node", "ty": "gen.grid", "settings": { "count": { "ron": "(2.0, 1.0, 1.0)" } } },
                { "op": "add_node", "ty": "spawn.mesh", "settings": { "entries": { "ron": "[(mesh: \"m.glb\")]" } } },
                { "op": "connect", "from": [1, "out"], "to": [2, "in"] },
            ], "save": true }),
        )
        .expect("edit");
        let good = w
            .spawn((
                ProcGenerator::new("g.pgraph.ron"),
                EntityGuid(regia_core::id::Guid(5)),
            ))
            .id();
        let bad = w
            .spawn((
                ProcGenerator::new("missing.pgraph.ron"),
                EntityGuid(regia_core::id::Guid(6)),
            ))
            .id();
        let _ = crate::procgen::run(w, &json!({ "entity": good.to_bits(), "wait": true }));
        let _ = crate::procgen::run(w, &json!({ "entity": bad.to_bits(), "wait": true }));
        app.update();
        let out = read(app.world(), &json!({})).expect("read");
        let records = out["records"].as_array().cloned().unwrap_or_default();
        let of = |guid: u128, kind: &str| -> Vec<Value> {
            records
                .iter()
                .filter(|r| {
                    r["kind"] == kind && r["data"]["generator"] == regia_core::id::Guid(guid).to_string()
                })
                .cloned()
                .collect()
        };
        let started = of(5, "procgen.run_started");
        let finished = of(5, "procgen.run_finished");
        assert_eq!((started.len(), finished.len()), (1, 1), "{records:?}");
        assert!(
            started[0]["seq"].as_u64() < finished[0]["seq"].as_u64(),
            "started first"
        );
        assert_eq!(started[0]["data"]["run_id"], finished[0]["data"]["run_id"]);
        assert_eq!(finished[0]["data"]["ok"], true);
        assert_eq!(finished[0]["data"]["counts"]["instances"], 2);
        let failed = of(6, "procgen.run_finished");
        assert_eq!(failed.len(), 1, "{records:?}");
        assert_eq!(failed[0]["data"]["ok"], false);
        assert!(failed[0]["data"]["code"].as_str().is_some_and(|c| !c.is_empty()));
        assert_eq!(
            of(6, "procgen.run_started")[0]["data"]["run_id"],
            failed[0]["data"]["run_id"]
        );
    }

    /// An app with the procgen plugin, the event tap and a small `rocks.pgraph.ron` in memory
    /// (`in.self` → `sample.region` → `spawn.mesh`), the same graph the tile tests use.
    fn rocks_app(editor: bool) -> App {
        use regia_procgen::world::{ProcGenPlugin, ProcGenSettings, ProcGraphSource};
        let n = |ty: &str, k: &str, v: &str| {
            let e = regia_procgen::NodeEntry::new(ty);
            if k.is_empty() {
                e
            } else {
                e.with(
                    k,
                    regia_procgen::SettingValue::parse(v).unwrap_or(regia_procgen::SettingValue::Int(0)),
                )
            }
        };
        let graph = regia_procgen::ProcGraph {
            nodes: [
                (1, n("in.self", "", "")),
                (2, n("sample.region", "spacing_m", "8.0")),
                (3, n("spawn.mesh", "entries", "[(mesh: \"rock.glb\")]")),
            ]
            .into_iter()
            .collect(),
            edges: vec![
                regia_procgen::Edge::new(1, "out", 2, "region"),
                regia_procgen::Edge::new(2, "out", 3, "in"),
            ],
            ..regia_procgen::ProcGraph::default()
        }
        .to_ron();
        let mut files = std::collections::BTreeMap::new();
        files.insert("rocks.pgraph.ron".to_string(), graph);
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(ProcGenSettings {
                inline: true,
                editor,
                source: ProcGraphSource::Memory(files),
                ..ProcGenSettings::default()
            })
            .add_plugins(ProcGenPlugin)
            .init_resource::<AgentEventLog>()
            .add_systems(Last, record_events)
            .insert_resource(TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_millis(16),
            ));
        app
    }

    fn records_of<'a>(out: &'a Value, guid: u128, kind: &str) -> Vec<&'a Value> {
        out["records"]
            .as_array()
            .map(|r| {
                r.iter()
                    .filter(|r| {
                        r["kind"] == kind && r["data"]["generator"] == regia_core::id::Guid(guid).to_string()
                    })
                    .map(|r| &r["data"])
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A queued run cancelled before it starts emits `procgen.run_cancelled` naming the generator
    /// and the run id the cancel answers (the run never emits `run_started`).
    #[test]
    fn cancelled_queued_run_emits_procgen_run_cancelled() {
        use regia_procgen::world::ProcGenerator;
        use regia_scene::components::EntityGuid;
        let mut app = rocks_app(false);
        let w = app.world_mut();
        let e = w
            .spawn((
                ProcGenerator::new("rocks.pgraph.ron"),
                EntityGuid(regia_core::id::Guid(7)),
            ))
            .id();
        let queued = crate::procgen::run(w, &json!({ "entity": e.to_bits() })).expect("queue");
        assert_eq!(queued["state"], "queued");
        let cancelled = crate::procgen::cancel(w, &json!({ "entity": e.to_bits() })).expect("cancel");
        assert_eq!(cancelled["cancelled"], true);
        app.update();
        let out = read(app.world(), &json!({})).expect("read");
        let events = records_of(&out, 7, "procgen.run_cancelled");
        assert_eq!(events.len(), 1, "{out}");
        assert_eq!(events[0]["run_id"], cancelled["run_id"]);
        assert_eq!(events[0]["entity"], e.to_bits());
        assert!(records_of(&out, 7, "procgen.run_started").is_empty(), "{out}");
    }

    /// `procgen.bake` on a `Baked` generator writes `procgen.bake_written` with the file paths.
    #[test]
    fn bake_emits_procgen_bake_written() {
        use regia_procgen::world::{ProcGenMode, ProcGenerator};
        use regia_scene::components::EntityGuid;
        let dir = regia_test_utils::unique_temp_dir("agent-events-bake");
        let mut app = rocks_app(false);
        app.world_mut()
            .resource_mut::<regia_procgen::world::ProcGenSettings>()
            .bake_dir = Some(dir.clone());
        let w = app.world_mut();
        let e = w
            .spawn((
                ProcGenerator {
                    mode: ProcGenMode::Baked,
                    ..ProcGenerator::new("rocks.pgraph.ron")
                },
                EntityGuid(regia_core::id::Guid(8)),
            ))
            .id();
        let baked = crate::procgen::bake(w, &json!({ "entity": e.to_bits() })).expect("bake");
        assert_eq!(baked["ok"], true, "{baked}");
        // `push_bake_written` writes the log directly; no frame needed.
        let out = read(app.world(), &json!({})).expect("read");
        let events = records_of(&out, 8, "procgen.bake_written");
        assert_eq!(events.len(), 1, "{out}");
        assert_eq!(events[0]["entity"], e.to_bits());
        assert!(
            events[0]["files"].as_array().is_some_and(|f| !f.is_empty()),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `procgen.clear` emits `procgen.cleared`; in the editor, `procgen.detach` emits
    /// `procgen.detached` with the new content root.
    #[test]
    fn clear_and_detach_emit_their_events() {
        use regia_procgen::world::ProcGenerator;
        use regia_scene::components::EntityGuid;
        let mut app = rocks_app(true);
        let w = app.world_mut();
        let e = w
            .spawn((
                ProcGenerator::new("rocks.pgraph.ron"),
                EntityGuid(regia_core::id::Guid(9)),
            ))
            .id();
        let bits = e.to_bits();
        crate::procgen::run(w, &json!({ "entity": bits, "wait": true })).expect("run");
        let cleared = crate::procgen::clear(w, &json!({ "entity": bits })).expect("clear");
        assert_eq!(cleared["cleared"], true);
        crate::procgen::run(w, &json!({ "entity": bits, "wait": true })).expect("re-run");
        let detached = crate::procgen::detach(w, &json!({ "entity": bits })).expect("detach");
        app.update();
        let out = read(app.world(), &json!({})).expect("read");
        let cleared_e = records_of(&out, 9, "procgen.cleared");
        assert_eq!(cleared_e.len(), 1, "{out}");
        assert_eq!(cleared_e[0]["entity"], bits);
        let detached_e = records_of(&out, 9, "procgen.detached");
        assert_eq!(detached_e.len(), 1, "{out}");
        assert_eq!(detached_e[0]["entity"], bits);
        assert_eq!(detached_e[0]["new_root"], detached["root"]);
        // Neither kind is a run event: no invented `run_id` (rescue review item 9).
        assert!(cleared_e[0].get("run_id").is_none(), "{out}");
        assert!(detached_e[0].get("run_id").is_none(), "{out}");
    }

    /// A tiled `Anchored` generator emits `procgen.cells_changed` with `added` cells as its cells
    /// turn resident around an anchor, and `released` when the anchor goes away.
    #[test]
    fn tiled_run_emits_cells_changed_when_cells_turn_over() {
        use regia_procgen::tiles::{AnchorSettings, AnchorSource, ProcGenAnchor, TileSettings};
        use regia_procgen::world::{ProcBounds, ProcGenMode, ProcGenerator};
        use regia_scene::Transform;
        use regia_scene::components::EntityGuid;
        let mut app = rocks_app(false);
        let generator = ProcGenerator {
            mode: ProcGenMode::Anchored,
            bounds: ProcBounds::Box {
                half_extents: regia_core::math::Vec3::new(512.0, 10.0, 512.0),
            },
            tiles: Some(TileSettings {
                cell_m: 64.0,
                halo_m: Some(4.0),
                runtime: Some(AnchorSettings {
                    anchors: AnchorSource::Tagged,
                    base_reach_m: 100.0,
                    ..AnchorSettings::default()
                }),
            }),
            seed: Some(5),
            ..ProcGenerator::new("rocks.pgraph.ron")
        };
        app.world_mut()
            .spawn((generator, EntityGuid(regia_core::id::Guid(10))));
        let anchor = app
            .world_mut()
            .spawn((
                ProcGenAnchor::default(),
                Transform::from_translation(regia_core::math::Vec3::new(10.0, 0.0, 10.0)),
            ))
            .id();
        for _ in 0..12 {
            app.update();
        }
        let out = read(app.world(), &json!({})).expect("read");
        let added = records_of(&out, 10, "procgen.cells_changed")
            .iter()
            .filter(|d| d["added"].as_array().is_some_and(|a| !a.is_empty()))
            .count();
        assert!(added > 0, "cells became resident: {out}");
        // With the anchor gone nothing is in reach, so every resident cell is released.
        app.world_mut().despawn(anchor);
        for _ in 0..12 {
            app.update();
        }
        let out = read(app.world(), &json!({})).expect("read");
        let released = records_of(&out, 10, "procgen.cells_changed")
            .iter()
            .filter(|d| d["released"].as_array().is_some_and(|a| !a.is_empty()))
            .count();
        assert!(released > 0, "cells were released: {out}");
    }
}
