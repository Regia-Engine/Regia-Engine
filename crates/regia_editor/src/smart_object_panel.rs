// SPDX-License-Identifier: MPL-2.0
//! The Smart Objects panel (`book/agents/editor-gameplay-panels.md`, ADR-2030): every smart object
//! and slot in the world the editor is showing, with each slot's claim state, holder and priority, an
//! object's off-reasons and definition, and the recent claim messages. It can switch objects and
//! slots off, take a claim back from its holder, and mark the slots in the viewport.
//!
//! The state is in [`SmartObjectPanel`], which the panel and `editor.smart_object_panel_*` share.
//! The rows read the components directly, so the edit world, which runs no smart object systems,
//! shows authored objects as well.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ai::smart_object::{SmartObject, SmartObjectSlot, smart_object_ancestor};
use regia_ai::smart_object_load::SmartObjectDefs;
use regia_ai::smart_object_v2::{SlotState, SmartObjectEvent, SmartObjectEvents, slot_state};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::audio_gizmo::Segment;
use crate::gameplay_panel::{
    CallRecord, LiveSource, PanelCall, entity_param, header, play_running, reason_error,
};
use crate::pie::PieSession;
use crate::viewport_agent::Handler;

/// Slots a snapshot lists at most; `slots_total` still counts them all.
const MAX_SLOTS: usize = 2048;

/// Messages a snapshot keeps.
const MAX_EVENTS: usize = 32;

/// The half size of a slot's marker in the viewport, metres.
const MARKER: f32 = 0.2;

/// The reason off-switches from the panel are recorded under.
const PANEL_REASON: &str = "editor_panel";

/// One slot.
#[derive(Clone, Debug, PartialEq)]
pub struct SlotRow {
    /// Its entity.
    pub entity: Entity,
    /// Its display name.
    pub name: String,
    /// Its role.
    pub role: String,
    /// `free`, `claimed`, `in_use` or `disabled`.
    pub state: &'static str,
    /// Who holds the claim.
    pub holder: Option<Entity>,
    /// The holder's display name.
    pub holder_name: Option<String>,
    /// The claim's priority (`low`, `normal`, ...); the last one when free.
    pub priority: &'static str,
    /// The claim generation, bumped when a claim ends.
    pub generation: u32,
    /// The slot's own enable flag.
    pub enabled: bool,
    /// Why it is switched off.
    pub off_reasons: Vec<String>,
    /// The index in the object's definition.
    pub def_slot: Option<u16>,
    /// World position.
    pub position: Option<[f32; 3]>,
}

/// One object and its slots.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectRow {
    /// Its entity; `None` gathers the slots with no object.
    pub entity: Option<Entity>,
    /// Its display name.
    pub name: String,
    /// Authored tags.
    pub tags: Vec<String>,
    /// `enabled` and no off-reason.
    pub enabled: bool,
    /// Why it is switched off.
    pub off_reasons: Vec<String>,
    /// The definition's path, when the object names a loaded one.
    pub definition: Option<String>,
    /// The definition's load failure.
    pub definition_error: Option<String>,
    /// Its slots, sorted by entity.
    pub slots: Vec<SlotRow>,
}

/// Slots by state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlotCounts {
    /// Free.
    pub free: usize,
    /// Claimed.
    pub claimed: usize,
    /// In use.
    pub in_use: usize,
    /// Disabled.
    pub disabled: usize,
}

/// The panel's model.
#[derive(Resource, Debug, Default)]
pub struct SmartObjectPanel {
    /// The world this snapshot came from.
    pub source: LiveSource,
    /// Objects sorted by entity, then the slots with no object.
    pub objects: Vec<ObjectRow>,
    /// Slots in the world, listed or not.
    pub slots_total: usize,
    /// Slots by state, of all of them.
    pub counts: SlotCounts,
    /// The recent messages, oldest first (needs the plugin in that world).
    pub events: Vec<String>,
    /// The chosen object or slot, marked in the viewport.
    pub selected: Option<Entity>,
    /// Mark the slots in the viewport.
    pub show_gizmo: bool,
    /// What the last call did.
    pub last_call: Option<CallRecord>,
    /// Snapshots taken so far.
    pub snapshots: u64,
    pending: Vec<PanelCall>,
}

fn event_text(world: &World, event: &SmartObjectEvent) -> String {
    let name = |entity: Entity| crate::outliner::display_name(world, entity);
    match event {
        SmartObjectEvent::Changed(c) => {
            let target = c.slot.or(c.object).map_or_else(|| "?".to_owned(), name);
            format!("{target}: {}", c.what.name())
        }
        SmartObjectEvent::Lost(l) => format!(
            "{} lost {}: {}",
            name(l.holder),
            name(l.handle.slot),
            l.reason.name()
        ),
    }
}

fn slot_row(world: &World, entity: Entity, slot: &SmartObjectSlot) -> SlotRow {
    SlotRow {
        entity,
        name: crate::outliner::display_name(world, entity),
        role: slot.role.clone(),
        state: slot_state(world, entity).map_or(SlotState::Disabled.name(), SlotState::name),
        holder: slot.claimed_by,
        holder_name: slot
            .claimed_by
            .map(|holder| crate::outliner::display_name(world, holder)),
        priority: slot.priority_held.name(),
        generation: slot.generation,
        enabled: slot.enabled,
        off_reasons: slot.off_reasons.iter().cloned().collect(),
        def_slot: slot.def_slot,
        position: crate::viewport::world_position(world, entity).map(|at| at.to_array()),
    }
}

impl SmartObjectPanel {
    /// Copies the objects and slots of `world` into the panel.
    pub fn snapshot(&mut self, world: &mut World, source: LiveSource) {
        let _span = tracing::debug_span!(target: "regia_editor", "smart_object_panel_snapshot").entered();
        if self.source != source {
            self.selected = None;
        }
        self.source = source;
        let mut object_query = world.query::<(Entity, &SmartObject)>();
        let mut objects: Vec<(Entity, &SmartObject)> = object_query.iter(world).collect();
        objects.sort_by_key(|(entity, _)| entity.to_bits());
        let defs = world.get_resource::<SmartObjectDefs>();
        let mut rows: Vec<ObjectRow> = objects
            .iter()
            .map(|(entity, object)| {
                let slot = object
                    .definition
                    .as_ref()
                    .and_then(|reference| defs.and_then(|defs| defs.slot(reference.id)));
                ObjectRow {
                    entity: Some(*entity),
                    name: crate::outliner::display_name(world, *entity),
                    tags: object.tags.clone(),
                    enabled: object.enabled && object.off_reasons.is_empty(),
                    off_reasons: object.off_reasons.iter().cloned().collect(),
                    definition: slot.and_then(|slot| slot.path.clone()).or_else(|| {
                        object
                            .definition
                            .as_ref()
                            .map(|reference| reference.id.to_string())
                    }),
                    definition_error: slot
                        .and_then(|slot| slot.error.as_ref())
                        .map(|error| format!("{} ({})", error.detail, error.reason)),
                    slots: Vec::new(),
                }
            })
            .collect();
        let mut loose = ObjectRow {
            entity: None,
            name: "(no object)".to_owned(),
            tags: Vec::new(),
            enabled: true,
            off_reasons: Vec::new(),
            definition: None,
            definition_error: None,
            slots: Vec::new(),
        };
        let mut slot_query = world.query::<(Entity, &SmartObjectSlot)>();
        let mut slots: Vec<(Entity, &SmartObjectSlot)> = slot_query.iter(world).collect();
        slots.sort_by_key(|(entity, _)| entity.to_bits());
        self.slots_total = slots.len();
        let mut counts = SlotCounts::default();
        for (index, (entity, slot)) in slots.iter().enumerate() {
            match slot_state(world, *entity) {
                Some(SlotState::Free) => counts.free += 1,
                Some(SlotState::Claimed) => counts.claimed += 1,
                Some(SlotState::InUse) => counts.in_use += 1,
                _ => counts.disabled += 1,
            }
            if index >= MAX_SLOTS {
                continue;
            }
            let row = slot_row(world, *entity, slot);
            let owner = smart_object_ancestor(world, *entity);
            match rows
                .iter_mut()
                .find(|object| object.entity == owner && owner.is_some())
            {
                Some(object) => object.slots.push(row),
                None => loose.slots.push(row),
            }
        }
        if !loose.slots.is_empty() {
            rows.push(loose);
        }
        self.objects = rows;
        self.counts = counts;
        self.events = world
            .get_resource::<SmartObjectEvents>()
            .map(|events| events.recent(MAX_EVENTS))
            .unwrap_or_default()
            .iter()
            .map(|event| event_text(world, event))
            .collect();
        if self
            .selected
            .is_some_and(|entity| world.get_entity(entity).is_err())
        {
            self.selected = None;
        }
        self.snapshots += 1;
    }

    /// Runs the queued calls on `world`.
    pub fn run_pending(&mut self, world: &mut World, source: LiveSource) {
        for call in std::mem::take(&mut self.pending) {
            let result = regia_agent::panel_calls::call(world, &call.method, &call.params);
            self.last_call = Some(CallRecord::of(&call.method, source, result));
        }
    }

    /// The panel as the agent reads it.
    #[must_use]
    pub fn json(&self) -> Value {
        let slot_json = |row: &SlotRow| {
            json!({
                "entity": row.entity.to_bits(),
                "name": row.name,
                "role": row.role,
                "state": row.state,
                "holder": row.holder.map(Entity::to_bits),
                "holder_name": row.holder_name,
                "priority": row.priority,
                "generation": row.generation,
                "enabled": row.enabled,
                "off_reasons": row.off_reasons,
                "def_slot": row.def_slot,
                "position": row.position,
            })
        };
        json!({
            "source": self.source.name(),
            "slots_total": self.slots_total,
            "truncated": self.slots_total > MAX_SLOTS,
            "counts": {
                "free": self.counts.free,
                "claimed": self.counts.claimed,
                "in_use": self.counts.in_use,
                "disabled": self.counts.disabled,
            },
            "objects": self.objects.iter().map(|object| json!({
                "entity": object.entity.map(Entity::to_bits),
                "name": object.name,
                "tags": object.tags,
                "enabled": object.enabled,
                "off_reasons": object.off_reasons,
                "definition": object.definition,
                "definition_error": object.definition_error,
                "slots": object.slots.iter().map(slot_json).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "events": self.events,
            "selected": self.selected.map(Entity::to_bits),
            "show_gizmo": self.show_gizmo,
            "snapshots": self.snapshots,
            "last_call": self.last_call.as_ref().map(CallRecord::json),
        })
    }

    fn overlay(&self) -> SmartObjectOverlay {
        SmartObjectOverlay {
            on: self.show_gizmo,
            selected: self.selected,
        }
    }
}

/// What the viewport marks, on the world it draws (the edit or the play world).
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SmartObjectOverlay {
    /// Draw the markers.
    pub on: bool,
    /// The chosen object or slot.
    pub selected: Option<Entity>,
}

fn state_color(state: SlotState) -> egui::Color32 {
    match state {
        SlotState::Free => egui::Color32::from_rgb(120, 220, 120),
        SlotState::Claimed => egui::Color32::from_rgb(235, 170, 80),
        SlotState::InUse => egui::Color32::from_rgb(230, 90, 90),
        SlotState::Disabled => egui::Color32::GRAY,
    }
}

/// Markers for the slots of `world`: a cross colored by state, larger when chosen, with a line to
/// the holder. Empty while the overlay is off.
#[must_use]
pub fn smart_object_gizmo_segments(world: &World) -> Vec<Segment> {
    let Some(overlay) = world.get_resource::<SmartObjectOverlay>().filter(|o| o.on) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entity_ref in world.iter_entities() {
        let Some(slot) = entity_ref.get::<SmartObjectSlot>() else {
            continue;
        };
        let entity = entity_ref.id();
        let Some(at) = crate::viewport::world_position(world, entity) else {
            continue;
        };
        let state = slot_state(world, entity).unwrap_or(SlotState::Disabled);
        let color = state_color(state);
        let chosen = overlay.selected == Some(entity)
            || overlay
                .selected
                .is_some_and(|object| smart_object_ancestor(world, entity) == Some(object));
        let size = if chosen { MARKER * 2.0 } else { MARKER };
        for axis in [Vec3::X, Vec3::Y, Vec3::Z] {
            out.push((at - axis * size, at + axis * size, color));
        }
        if let Some(holder) = slot
            .claimed_by
            .and_then(|h| crate::viewport::world_position(world, h))
        {
            out.push((at, holder, color));
        }
    }
    out
}

/// Draws the panel. With no play session it looks at the edit world.
pub fn smart_object_panel(ui: &mut egui::Ui, world: &mut World) {
    let mut panel = world.remove_resource::<SmartObjectPanel>().unwrap_or_default();
    if !play_running(world) {
        panel.snapshot(world, LiveSource::Edit);
    }
    let open = draw(ui, &mut panel);
    world.insert_resource(panel.overlay());
    world.insert_resource(panel);
    // "Open definition" (ADR-7736): the object's file in the definition editor, the object as its
    // preview entity.
    if let Some((entity, path)) = open {
        match crate::smart_object_def_panel::open_def(world, &path) {
            Ok(()) => {
                world
                    .get_resource_or_insert_with(crate::smart_object_def_panel::SmartObjectDefPanel::default)
                    .preview = entity;
            }
            Err(e) => tracing::warn!(target: "regia_editor", "open definition {path}: {}", e.message),
        }
    }
}

/// Editor-loop hook, once per frame: runs the queue on the play world, copies its slots and passes
/// the overlay on, or runs the queue on the edit world when nothing plays.
pub fn pump(edit: &mut World, session: Option<&mut PieSession>) {
    let _span = tracing::debug_span!(target: "regia_editor", "smart_object_panel_pump").entered();
    let Some(mut panel) = edit.remove_resource::<SmartObjectPanel>() else {
        return;
    };
    match session {
        Some(session) => {
            let play = session.app.world_mut();
            panel.run_pending(play, LiveSource::Play);
            panel.snapshot(play, LiveSource::Play);
            play.insert_resource(panel.overlay());
        }
        None => panel.run_pending(edit, LiveSource::Edit),
    }
    edit.insert_resource(panel.overlay());
    edit.insert_resource(panel);
}

/// Draws the rows; returns an "Open definition" press (the object and its definition's project path).
fn draw(ui: &mut egui::Ui, panel: &mut SmartObjectPanel) -> Option<(Option<Entity>, String)> {
    #[cfg(test)]
    tests::DRAWN.with(|drawn| drawn.borrow_mut().clear());
    let c = panel.counts;
    let detail = format!(
        "{} slots: {} free, {} claimed, {} in use, {} off",
        panel.slots_total, c.free, c.claimed, c.in_use, c.disabled
    );
    header(ui, "Smart Objects", panel.source, &detail);
    ui.checkbox(&mut panel.show_gizmo, "Mark the slots in the viewport");
    if let Some(record) = &panel.last_call {
        record.draw(ui);
    }
    if panel.objects.is_empty() {
        ui.weak("No smart object here. Add SmartObject and SmartObjectSlot in the Inspector.");
        return None;
    }
    let mut calls = Vec::new();
    let mut open = None;
    let mut chosen = panel.selected;
    // Blocks are one to many lines high (a header, a load error, a line per slot): only the
    // blocks in view are laid out (ADR-7326).
    let line = ui.spacing().interact_size.y;
    let spacing = ui.spacing().item_spacing.y;
    let objects = &panel.objects;
    let max_height = (ui.available_height() - 120.0).max(80.0);
    crate::virtual_list::variable_rows(
        ui,
        "smart-object-list",
        objects.len(),
        max_height,
        |index| {
            let object = &objects[index];
            let lines = 1 + usize::from(object.definition_error.is_some()) + object.slots.len();
            lines as f32 * line + (lines - 1) as f32 * spacing
        },
        |ui, index| object_block(ui, &objects[index], &mut chosen, &mut calls, &mut open),
    );
    panel.selected = chosen;
    panel.pending.extend(calls);
    ui.separator();
    ui.weak("Recent messages");
    egui::ScrollArea::vertical()
        .id_salt("smart-object-events")
        .show(ui, |ui| {
            for line in panel.events.iter().rev() {
                ui.monospace(line);
            }
        });
    open
}

fn object_block(
    ui: &mut egui::Ui,
    object: &ObjectRow,
    chosen: &mut Option<Entity>,
    calls: &mut Vec<PanelCall>,
    open: &mut Option<(Option<Entity>, String)>,
) {
    ui.horizontal(|ui| {
        let selected = object.entity.is_some() && *chosen == object.entity;
        let label = ui.selectable_label(selected, egui::RichText::new(&object.name).strong());
        #[cfg(test)]
        tests::DRAWN.with(|drawn| drawn.borrow_mut().push((object.entity, label.rect)));
        if label.clicked() {
            *chosen = object.entity;
        }
        if let Some(entity) = object.entity {
            enable_switch(ui, entity, object.enabled, calls);
        }
        if !object.tags.is_empty() {
            ui.weak(object.tags.join(", "));
        }
        if !object.off_reasons.is_empty() {
            ui.weak(format!("off: {}", object.off_reasons.join(", ")));
        }
        if let Some(definition) = &object.definition {
            ui.weak(definition);
            if definition.ends_with(crate::smart_object_def_panel::SUFFIX)
                && ui.small_button("Open definition").clicked()
            {
                let path = if definition.starts_with("assets/") {
                    definition.clone()
                } else {
                    format!("assets/{definition}")
                };
                *open = Some((object.entity, path));
            }
        }
    });
    if let Some(error) = &object.definition_error {
        ui.colored_label(egui::Color32::from_rgb(230, 90, 90), error);
    }
    for slot in &object.slots {
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            let label = format!("{} ({})", slot.name, slot.role);
            if ui.selectable_label(*chosen == Some(slot.entity), label).clicked() {
                *chosen = Some(slot.entity);
            }
            let color = match slot.state {
                "free" => state_color(SlotState::Free),
                "claimed" => state_color(SlotState::Claimed),
                "in_use" => state_color(SlotState::InUse),
                _ => state_color(SlotState::Disabled),
            };
            ui.colored_label(color, slot.state);
            enable_switch(ui, slot.entity, slot.off_reasons.is_empty(), calls);
            if let (Some(holder), Some(name)) = (slot.holder, &slot.holder_name) {
                ui.label(format!("held by {name} at {}", slot.priority));
                if ui.small_button("Release").clicked() {
                    calls.push(release_call(slot.entity, holder));
                }
            }
            if !slot.off_reasons.is_empty() {
                ui.weak(format!("off: {}", slot.off_reasons.join(", ")));
            }
        });
    }
}

fn enable_switch(ui: &mut egui::Ui, entity: Entity, on: bool, calls: &mut Vec<PanelCall>) {
    let mut value = on;
    if ui.checkbox(&mut value, "on").changed() {
        calls.push(set_enabled_call(entity, value, PANEL_REASON));
    }
}

fn set_enabled_call(entity: Entity, enabled: bool, reason: &str) -> PanelCall {
    PanelCall::new(
        "smartobject.set_enabled",
        json!({ "entity": entity.to_bits(), "enabled": enabled, "reason": reason }),
    )
}

fn release_call(slot: Entity, holder: Entity) -> PanelCall {
    PanelCall::new(
        "smartobject.release",
        json!({ "slot": slot.to_bits(), "agent": holder.to_bits() }),
    )
}

fn with_panel<T>(world: &mut World, f: impl FnOnce(&mut SmartObjectPanel, &mut World) -> T) -> T {
    let mut panel = world.remove_resource::<SmartObjectPanel>().unwrap_or_default();
    let out = f(&mut panel, world);
    world.insert_resource(panel.overlay());
    world.insert_resource(panel);
    out
}

/// `editor.smart_object_panel_get`.
fn panel_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(with_panel(world, |panel, world| {
        if !play_running(world) {
            panel.snapshot(world, LiveSource::Edit);
        }
        panel.json()
    }))
}

/// `editor.smart_object_panel_set { selected?, show_gizmo? }`.
fn panel_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let selected = entity_param(params, "selected")?;
    with_panel(world, |panel, world| {
        let playing = play_running(world);
        if !playing {
            panel.snapshot(world, LiveSource::Edit);
        }
        if let Some(entity) = selected {
            let listed = panel.objects.iter().any(|object| {
                object.entity == Some(entity) || object.slots.iter().any(|slot| slot.entity == entity)
            });
            if !listed {
                return Err(reason_error(
                    "entity_not_found",
                    "the entity is neither a listed smart object nor a slot",
                ));
            }
            panel.selected = Some(entity);
        }
        match params.get("show_gizmo") {
            None | Some(Value::Null) => {}
            Some(value) => {
                panel.show_gizmo = value
                    .as_bool()
                    .ok_or_else(|| RpcError::invalid_params("`show_gizmo` must be a boolean"))?;
            }
        }
        Ok(panel.json())
    })
}

/// The call an `op` stands for. A release finds the holder in the snapshot when none is given.
fn call_of(panel: &SmartObjectPanel, params: &Value) -> Result<PanelCall, RpcError> {
    let entity = entity_param(params, "entity")?
        .or(panel.selected)
        .ok_or_else(|| {
            reason_error(
                "entity_not_found",
                "nothing is selected and no `entity` was given",
            )
        })?;
    match params.get("op").and_then(Value::as_str).unwrap_or_default() {
        "set_enabled" => {
            let enabled = params
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| reason_error("missing_param", "`enabled` (a boolean) is required"))?;
            let reason = params
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or(PANEL_REASON);
            Ok(set_enabled_call(entity, enabled, reason))
        }
        "release" => {
            let holder = panel
                .objects
                .iter()
                .flat_map(|object| &object.slots)
                .find(|slot| slot.entity == entity)
                .and_then(|slot| slot.holder)
                .ok_or_else(|| reason_error("slot_not_claimed", "the slot has no holder"))?;
            Ok(release_call(entity, holder))
        }
        other => Err(reason_error(
            "bad_op",
            format!("unknown op {other:?}: set_enabled or release"),
        )),
    }
}

/// `editor.smart_object_panel_control { entity?, op, enabled?, reason? }`.
fn panel_control(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    with_panel(world, |panel, world| {
        let playing = play_running(world);
        if !playing {
            panel.snapshot(world, LiveSource::Edit);
        }
        let call = call_of(panel, params)?;
        if playing {
            panel.pending.push(call);
            return Ok(json!({
                "queued": true,
                "hint": "applied to the play world on the next frame; read last_call from editor.smart_object_panel_get",
            }));
        }
        let result = regia_agent::panel_calls::call(world, &call.method, &call.params);
        panel.last_call = Some(CallRecord::of(&call.method, LiveSource::Edit, result.clone()));
        panel.snapshot(world, LiveSource::Edit);
        Ok(json!({ "queued": false, "answer": result? }))
    })
}

const fn param(name: &'static str, kind: &'static str, description: &'static str) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required: false,
        description,
    }
}

const SET_PARAMS: &[ParamSchema] = &[
    param("selected", "integer", "The object or slot to mark."),
    param("show_gizmo", "boolean", "Mark the slots in the viewport."),
];

const CONTROL_PARAMS: &[ParamSchema] = &[
    param(
        "entity",
        "integer",
        "The object or slot; default the selected one.",
    ),
    ParamSchema {
        name: "op",
        kind: "string",
        required: true,
        description: "set_enabled or release (takes the claim back from its holder).",
    },
    param("enabled", "boolean", "set_enabled: on or off."),
    param(
        "reason",
        "string",
        "set_enabled: the off-reason; default editor_panel.",
    ),
];

const STATE: &str = "{ source, slots_total, truncated, counts: { free, claimed, in_use, disabled }, objects: [{ entity, name, \
    tags, enabled, off_reasons, definition, definition_error, slots: [{ entity, name, role, state, holder, holder_name, \
    priority, generation, enabled, off_reasons, def_slot, position }] }], events, selected, show_gizmo, snapshots, last_call }";

/// The Smart Objects panel's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.smart_object_panel_get",
                tool: "smart_object_panel_get",
                description: "The Smart Objects panel: objects and slots of the play world (or of the edit world \
                    when nothing plays) with each slot's claim state, holder and priority, and the recent messages.",
                params: &[],
                result: STATE,
                mutating: false,
                permission: Permission::Read,
            },
            panel_get as Handler,
        ),
        (
            MethodSchema {
                name: "editor.smart_object_panel_set",
                tool: "smart_object_panel_set",
                description: "Choose the object or slot the panel marks and turn the viewport markers on or off.",
                params: SET_PARAMS,
                result: STATE,
                mutating: true,
                permission: Permission::Play,
            },
            panel_set as Handler,
        ),
        (
            MethodSchema {
                name: "editor.smart_object_panel_control",
                tool: "smart_object_panel_control",
                description: "Act as the panel's switches do: set_enabled on an object or slot, or release a slot's \
                    claim. While playing the call runs on the play world on the next frame (queued: true) and its \
                    result is last_call.",
                params: CONTROL_PARAMS,
                result: "{ queued, answer?, hint? }",
                mutating: true,
                permission: Permission::Play,
            },
            panel_control as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use regia_ai::smart_object::{claim_slot, effective_enabled};
    use regia_ai::smart_object_v2::{ClaimPriority, begin_use, claim_with};
    use regia_core::math::Affine3A;
    use regia_scene::prelude::{GlobalTransform, Name, Transform};

    use super::*;
    use crate::gameplay_panel::reason_of;

    struct Scene {
        world: World,
        chair: Entity,
        seat: Entity,
        back: Entity,
        guard: Entity,
    }

    fn scene() -> Scene {
        let mut world = World::new();
        // The index `SmartObjectPlugin` installs: the panel's release goes through
        // `smartobject.release`, which answers NOT_FOUND without it (spec 31, 41832aca2).
        world.init_resource::<regia_ai::SmartObjectIndex>();
        let chair = world
            .spawn((
                Name::new("Chair"),
                Transform::default(),
                SmartObject {
                    tags: vec!["furniture".into()],
                    ..SmartObject::default()
                },
            ))
            .id();
        let seat = world
            .spawn((
                Name::new("Seat"),
                Transform::default(),
                // No propagation runs in this world, so the world pose is set directly.
                GlobalTransform(Affine3A::from_translation(Vec3::new(1.0, 0.0, 0.0))),
                SmartObjectSlot::new("sit"),
                regia_ecs::ecs::hierarchy::ChildOf(chair),
            ))
            .id();
        let back = world
            .spawn((
                Name::new("Back"),
                Transform::default(),
                SmartObjectSlot::new("lean"),
                regia_ecs::ecs::hierarchy::ChildOf(chair),
            ))
            .id();
        let guard = world
            .spawn((
                Name::new("Guard"),
                Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)),
            ))
            .id();
        Scene {
            world,
            chair,
            seat,
            back,
            guard,
        }
    }

    fn get(world: &mut World) -> Value {
        panel_get(world, &json!({})).expect("get")
    }

    fn slot(view: &Value, name: &str) -> Value {
        view["objects"]
            .as_array()
            .expect("objects")
            .iter()
            .flat_map(|o| o["slots"].as_array().expect("slots"))
            .find(|s| s["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no slot {name}"))
    }

    #[test]
    fn the_view_lists_slots_by_object_with_claim_state() {
        let mut s = scene();
        let v = get(&mut s.world);
        assert_eq!(v["source"], "edit");
        assert_eq!(v["objects"].as_array().expect("objects").len(), 1);
        assert_eq!(v["objects"][0]["name"], "Chair");
        assert_eq!(v["objects"][0]["tags"], json!(["furniture"]));
        assert_eq!(v["counts"]["free"], 2);
        assert_eq!(slot(&v, "Seat")["state"], "free");
        assert_eq!(slot(&v, "Seat")["role"], "sit");
        assert_eq!(slot(&v, "Seat")["position"], json!([1.0, 0.0, 0.0]));

        claim_slot(&mut s.world, s.seat, s.guard).expect("claim");
        let v = get(&mut s.world);
        assert_eq!(slot(&v, "Seat")["state"], "claimed");
        assert_eq!(slot(&v, "Seat")["holder"], s.guard.to_bits());
        assert_eq!(slot(&v, "Seat")["holder_name"], "Guard");
        assert_eq!(slot(&v, "Seat")["priority"], "normal");
        assert_eq!(v["counts"]["claimed"], 1);

        let handle = claim_with(&mut s.world, s.back, s.guard, ClaimPriority::High).expect("claim");
        begin_use(&mut s.world, handle).expect("use");
        let v = get(&mut s.world);
        assert_eq!(slot(&v, "Back")["state"], "in_use");
        assert_eq!(slot(&v, "Back")["priority"], "high");
        assert_eq!(v["counts"]["in_use"], 1);
        assert_eq!(v["counts"]["free"], 0);
    }

    #[test]
    fn a_slot_with_no_object_is_listed_apart() {
        let mut world = World::new();
        world.spawn((Name::new("Lone"), SmartObjectSlot::new("wait")));
        let v = get(&mut world);
        assert_eq!(v["objects"][0]["entity"], Value::Null);
        assert_eq!(slot(&v, "Lone")["state"], "free");
    }

    #[test]
    fn switches_and_release_change_the_world_and_keep_their_reasons() {
        let mut s = scene();
        get(&mut s.world);
        let chair = s.chair.to_bits();
        panel_control(
            &mut s.world,
            &json!({ "op": "set_enabled", "entity": chair, "enabled": false }),
        )
        .expect("off");
        let v = get(&mut s.world);
        assert_eq!(v["objects"][0]["enabled"], false);
        assert_eq!(v["objects"][0]["off_reasons"], json!(["editor_panel"]));
        assert_eq!(
            slot(&v, "Seat")["state"],
            "disabled",
            "the object switches its slots off"
        );
        panel_control(
            &mut s.world,
            &json!({ "op": "set_enabled", "entity": chair, "enabled": true, "reason": "other" }),
        )
        .expect("a different reason");
        assert_eq!(
            get(&mut s.world)["objects"][0]["enabled"],
            false,
            "two reasons, two enables"
        );
        panel_control(
            &mut s.world,
            &json!({ "op": "set_enabled", "entity": chair, "enabled": true }),
        )
        .expect("on");
        assert_eq!(get(&mut s.world)["objects"][0]["enabled"], true);

        claim_slot(&mut s.world, s.seat, s.guard).expect("claim");
        // The selected slot is the default target.
        panel_set(&mut s.world, &json!({ "selected": s.seat.to_bits() })).expect("select");
        panel_control(&mut s.world, &json!({ "op": "release" })).expect("release");
        assert_eq!(slot(&get(&mut s.world), "Seat")["state"], "free");
        let last = get(&mut s.world)["last_call"].clone();
        assert_eq!(last["ok"], true);
        assert_eq!(last["answer"]["released"], true);
    }

    #[test]
    fn errors_carry_their_codes() {
        let mut s = scene();
        let reason = |r: Result<Value, RpcError>| reason_of(&r.expect_err("refused")).unwrap_or_default();
        assert_eq!(
            reason(panel_control(&mut s.world, &json!({ "op": "release" }))),
            "entity_not_found"
        );
        let seat = s.seat.to_bits();
        assert_eq!(
            reason(panel_control(
                &mut s.world,
                &json!({ "op": "release", "entity": seat })
            )),
            "slot_not_claimed"
        );
        assert_eq!(
            reason(panel_control(&mut s.world, &json!({ "op": "x", "entity": seat }))),
            "bad_op"
        );
        assert_eq!(
            reason(panel_control(
                &mut s.world,
                &json!({ "op": "set_enabled", "entity": seat })
            )),
            "missing_param"
        );
        let guard = s.guard.to_bits();
        assert_eq!(
            reason(panel_set(&mut s.world, &json!({ "selected": guard }))),
            "entity_not_found"
        );
        assert_eq!(
            reason(panel_control(
                &mut s.world,
                &json!({ "op": "set_enabled", "entity": guard, "enabled": false })
            )),
            "entity_not_found"
        );
    }

    #[test]
    fn the_viewport_marks_slots_by_state_only_when_asked() {
        let mut s = scene();
        get(&mut s.world);
        assert!(smart_object_gizmo_segments(&s.world).is_empty(), "off by default");
        panel_set(&mut s.world, &json!({ "show_gizmo": true })).expect("on");
        let idle = smart_object_gizmo_segments(&s.world);
        assert_eq!(idle.len(), 6, "a cross for each of two slots");
        assert!(
            idle.iter()
                .all(|segment| segment.2 == state_color(SlotState::Free))
        );
        claim_slot(&mut s.world, s.seat, s.guard).expect("claim");
        let claimed = smart_object_gizmo_segments(&s.world);
        assert_eq!(claimed.len(), 7, "and a line to the holder");
        assert!(
            claimed
                .iter()
                .any(|segment| segment.2 == state_color(SlotState::Claimed))
        );
        // Choosing the object makes its slots' crosses larger.
        panel_set(&mut s.world, &json!({ "selected": s.chair.to_bits() })).expect("select");
        let chosen = smart_object_gizmo_segments(&s.world);
        let length = |segment: &Segment| (segment.1 - segment.0).length();
        assert!(
            chosen
                .iter()
                .any(|segment| (length(segment) - MARKER * 4.0).abs() < 1e-4)
        );
    }

    #[test]
    fn while_playing_calls_queue_and_the_snapshot_comes_from_play() {
        let mut s = scene();
        let mut edit = World::new();
        edit.insert_resource(crate::pie::PieStatus {
            mode: Some(crate::pie::PlayMode::Playing),
            ..Default::default()
        });
        let pump_once = |edit: &mut World, play: &mut World| {
            let mut panel = edit.remove_resource::<SmartObjectPanel>().unwrap_or_default();
            panel.run_pending(play, LiveSource::Play);
            panel.snapshot(play, LiveSource::Play);
            edit.insert_resource(panel);
        };
        pump_once(&mut edit, &mut s.world);
        let v = get(&mut edit);
        assert_eq!(
            (v["source"].clone(), v["slots_total"].clone()),
            (json!("play"), json!(2))
        );
        let seat = s.seat.to_bits();
        let queued = panel_control(
            &mut edit,
            &json!({ "op": "set_enabled", "entity": seat, "enabled": false }),
        )
        .expect("queued");
        assert_eq!(queued["queued"], true);
        assert_eq!(slot(&get(&mut edit), "Seat")["state"], "free", "not applied yet");
        pump_once(&mut edit, &mut s.world);
        let v = get(&mut edit);
        assert_eq!(slot(&v, "Seat")["state"], "disabled");
        assert_eq!(v["last_call"]["source"], "play");
        assert!(effective_enabled(&s.world, s.back));
    }

    thread_local! {
        /// The object blocks the last draw laid out, with their name label's rect.
        pub(super) static DRAWN: std::cell::RefCell<Vec<(Option<Entity>, egui::Rect)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    /// 5 000 objects of one to four rows each lay out only the blocks in view, and a click on a
    /// block's name after scrolling selects that object (ADR-7326).
    #[test]
    fn five_thousand_objects_lay_out_the_blocks_in_view_and_a_click_after_scrolling_selects() {
        let mut world = World::new();
        let row = |entity: Entity, name: String| SlotRow {
            entity,
            name,
            role: "sit".into(),
            state: "free",
            holder: None,
            holder_name: None,
            priority: "normal",
            generation: 0,
            enabled: true,
            off_reasons: Vec::new(),
            def_slot: None,
            position: None,
        };
        let mut panel = SmartObjectPanel::default();
        panel.objects = (0..5_000)
            .map(|i| {
                let entity = world.spawn_empty().id();
                let slots = (0..i % 4)
                    .map(|s| row(world.spawn_empty().id(), format!("slot {i}.{s}")))
                    .collect();
                ObjectRow {
                    entity: Some(entity),
                    name: format!("Object {i}"),
                    tags: Vec::new(),
                    enabled: true,
                    off_reasons: Vec::new(),
                    definition: None,
                    definition_error: (i % 7 == 0).then(|| "a long load failure".repeat(3)),
                    slots,
                }
            })
            .collect();
        panel.slots_total = panel.objects.iter().map(|o| o.slots.len()).sum();
        let ctx = egui::Context::default();
        let frame = |panel: &mut SmartObjectPanel, events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 800.0),
                )),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                draw(ui, panel);
            });
            output.textures_delta.clear();
            DRAWN.with(|drawn| drawn.borrow().clone())
        };
        frame(&mut panel, Vec::new());
        let drawn = frame(&mut panel, Vec::new());
        assert_eq!(drawn.first().map(|(e, _)| *e), Some(panel.objects[0].entity));
        assert!(drawn.len() < 60, "a screenful of 5 000 blocks: {}", drawn.len());

        // Scroll down with the wheel over the list.
        let inside = drawn[0].1.center();
        frame(&mut panel, vec![egui::Event::PointerMoved(inside)]);
        frame(
            &mut panel,
            vec![egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -6_000.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        let mut drawn = Vec::new();
        for _ in 0..120 {
            drawn = frame(&mut panel, Vec::new());
        }
        let first = drawn.first().and_then(|(e, _)| *e).expect("a block in view");
        let first_index = panel
            .objects
            .iter()
            .position(|o| o.entity == Some(first))
            .expect("listed");
        assert!(first_index > 20, "scrolled past the top: {first_index}");
        assert!(drawn.len() < 60, "still a screenful: {}", drawn.len());

        // Click the name of a block in the middle of the view.
        let (target, rect) = drawn[drawn.len() / 2];
        let at = rect.center();
        let press = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(&mut panel, vec![egui::Event::PointerMoved(at)]);
        frame(&mut panel, vec![press(true)]);
        frame(&mut panel, vec![press(false)]);
        assert_eq!(panel.selected, target, "the clicked block is selected");
        let after = frame(&mut panel, Vec::new());
        assert_eq!(
            after.first().and_then(|(e, _)| *e),
            Some(first),
            "the click did not scroll"
        );
    }

    #[test]
    fn the_panel_draws_headless_in_every_state() {
        let draw_frames = |world: &mut World| {
            let ctx = egui::Context::default();
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 800.0),
                )),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| smart_object_panel(ui, world));
            output.textures_delta.clear();
            ctx.tessellate(output.shapes, output.pixels_per_point).len()
        };
        let mut empty = World::new();
        assert!(draw_frames(&mut empty) > 0, "nothing");
        let mut s = scene();
        assert!(draw_frames(&mut s.world) > 0, "free slots");
        claim_slot(&mut s.world, s.seat, s.guard).expect("claim");
        panel_control(
            &mut s.world,
            &json!({ "op": "set_enabled", "entity": s.back.to_bits(), "enabled": false }),
        )
        .expect("off");
        let plain = s.guard.to_bits();
        panel_control(
            &mut s.world,
            &json!({ "op": "set_enabled", "entity": plain, "enabled": false }),
        )
        .expect_err("refused");
        assert!(draw_frames(&mut s.world) > 0, "claimed, off, a failed call");
    }

    #[test]
    fn every_method_answers_by_its_registered_name() {
        let list = methods();
        let mut s = scene();
        for (schema, handler) in &list {
            let params = match schema.name {
                "editor.smart_object_panel_control" => {
                    json!({ "op": "set_enabled", "entity": s.seat.to_bits(), "enabled": true })
                }
                _ => json!({}),
            };
            handler(&mut s.world, &params).unwrap_or_else(|e| panic!("{}: {e:?}", schema.name));
        }
        assert_eq!(list.len(), 3);
    }
}
