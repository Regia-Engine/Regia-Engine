// SPDX-License-Identifier: MPL-2.0
//! Panel instances (parity row P-003, ADR-8105 (a)): several Outliners, Content Browsers and
//! Inspectors, each with its own state.
//!
//! Instance 0 of a panel is its plain [`Panel`] variant; Window ▸ New
//! Outliner (New Content Browser, New Inspector), the palette commands `window.new_outliner` /
//! `window.new_content` / `window.new_inspector` and `editor.panel_new_instance { panel }` open
//! instance `n` (the lowest free of `1..=7`) as
//! [`Panel::Instance`], whose layout id is `<panel>#<n>`. Each
//! instance has:
//!
//! - the Outliner: its own search text and row cache, and its own columns (the tree
//!   `outliner#<n>` of the Outliner view state; instance 0 keeps the `main` tree);
//! - the Content Browser: its own folder and search
//!   ([`ShellInstances`]);
//! - the Inspector: its own lock
//!   ([`InspectorInstanceLocks`]).
//!
//! The shell publishes what each open instance shows into
//! [`PanelInstanceStates`] every frame; the layout
//! file (`editor-layout` version 3) saves it as `instances` and a restore gives it back. Closing
//! an instance drops its state: a later instance with the same number starts fresh.
//!
//! While an instance draws, [`DrawingInstance`] names it
//! so the panel's widget ids and state keys ([`scoped_id`],
//! [`outliner_tree`]) are its own: two Outliners side by
//! side never share a search box, a focus or a row cache.

use std::collections::BTreeMap;

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::content::ContentBrowserState;
use crate::editor_layout::{LayoutRequest, LayoutRequests, LayoutSnapshot, PanelInstanceState};
use crate::shell::{InstancedPanel, MAX_PANEL_INSTANCE, Panel};

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

/// The egui temp-data key of an Outliner instance's search text (scoped by [`scoped_id`]).
pub const OUTLINER_FILTER_KEY: &str = "outliner-filter";

/// The palette commands that open a new instance, by panel.
pub const NEW_INSTANCE_COMMANDS: [(&str, InstancedPanel); 3] = [
    ("window.new_outliner", InstancedPanel::Outliner),
    ("window.new_content", InstancedPanel::Content),
    ("window.new_inspector", InstancedPanel::Inspector),
];

/// The instance number of the panel being drawn in this world (0 outside an instance's draw).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrawingInstance(pub u8);

/// The instance number of the panel being drawn (0 when none or instance 0).
#[must_use]
pub fn drawing_instance(world: &World) -> u8 {
    world.get_resource::<DrawingInstance>().map_or(0, |d| d.0)
}

/// The id `base` of instance `n`: `base` itself for instance 0 (so the ids of a single panel are
/// unchanged), `(base, n)` for instance `n`.
#[must_use]
pub fn instance_id(base: &str, n: u8) -> egui::Id {
    match n {
        0 => egui::Id::new(base),
        n => egui::Id::new((base, n)),
    }
}

/// The widget or state id `base` of the panel being drawn ([`instance_id`] of [`drawing_instance`]).
#[must_use]
pub fn scoped_id(world: &World, base: &str) -> egui::Id {
    instance_id(base, drawing_instance(world))
}

/// The Outliner columns tree of the Outliner being drawn: `main` for instance 0,
/// `outliner#<n>` for instance `n`.
#[must_use]
pub fn outliner_tree(world: &World) -> &'static str {
    match drawing_instance(world) {
        0 => crate::outliner_columns::MAIN_TREE,
        n => Panel::Instance(InstancedPanel::Outliner, n).id(),
    }
}

/// What every open instance shows, by tab id (`outliner`, `outliner#1`, …), as the layout saves it.
/// The shell publishes it each frame; [`PanelInstanceStates::revision`] moves when it changes.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct PanelInstanceStates {
    /// The non-empty states by tab id.
    pub states: BTreeMap<String, PanelInstanceState>,
    /// Bumped by every change.
    pub revision: u64,
}

impl PanelInstanceStates {
    /// Replaces the published states when they differ; returns whether they did.
    pub fn publish(world: &mut World, states: BTreeMap<String, PanelInstanceState>) -> bool {
        let mut current = world.get_resource_or_insert_with(Self::default);
        if current.states == states {
            return false;
        }
        current.states = states;
        current.revision += 1;
        true
    }

    /// The published revision (0 before the first publish).
    #[must_use]
    pub fn revision_of(world: &World) -> u64 {
        world.get_resource::<Self>().map_or(0, |s| s.revision)
    }
}

/// The locks of Inspector instances `1..=7` (instance 0's is
/// [`crate::inspector::pinned::InspectorLock`]).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct InspectorInstanceLocks(pub BTreeMap<u8, Entity>);

/// The row caches of Outliner instances `1..=7` (instance 0's are the world's own
/// [`crate::outliner::OutlinerView`] and [`crate::outliner::OutlinerRows`]).
#[derive(Resource, Default)]
struct OutlinerCaches(
    BTreeMap<
        u8,
        (
            Option<crate::outliner::OutlinerView>,
            Option<crate::outliner::OutlinerRows>,
        ),
    >,
);

/// Swaps resource `R` with `slot`.
fn swap_resource<R: Resource>(world: &mut World, slot: &mut Option<R>) {
    let current = world.remove_resource::<R>();
    if let Some(next) = slot.take() {
        world.insert_resource(next);
    }
    *slot = current;
}

/// Draws `panel` with `draw` in `world` as its own instance: [`DrawingInstance`] names it, and an
/// Outliner instance's row cache or an Inspector instance's lock is swapped in for the draw and
/// back out after it (instance 0 draws with the world's own state).
pub fn with_instance(world: &mut World, panel: Panel, draw: impl FnOnce(&mut World)) {
    let n = panel.instance();
    if n == 0 {
        draw(world);
        return;
    }
    let before = world.get_resource::<DrawingInstance>().copied();
    world.insert_resource(DrawingInstance(n));
    match InstancedPanel::of(panel) {
        Some(InstancedPanel::Outliner) => {
            let mut caches = world.remove_resource::<OutlinerCaches>().unwrap_or_default();
            let (mut view, mut rows) = caches.0.remove(&n).unwrap_or_default();
            swap_resource(world, &mut view);
            swap_resource(world, &mut rows);
            draw(world);
            swap_resource(world, &mut view);
            swap_resource(world, &mut rows);
            caches.0.insert(n, (view, rows));
            world.insert_resource(caches);
        }
        Some(InstancedPanel::Inspector) => {
            use crate::inspector::pinned::InspectorLock;
            let mine = world
                .get_resource::<InspectorInstanceLocks>()
                .and_then(|locks| locks.0.get(&n).copied());
            let main = world.get_resource::<InspectorLock>().copied();
            world.insert_resource(InspectorLock { entity: mine });
            draw(world);
            let after = world.get_resource::<InspectorLock>().and_then(|lock| lock.entity);
            let mut locks = world.get_resource_or_insert_with(InspectorInstanceLocks::default);
            match after {
                Some(entity) => locks.0.insert(n, entity),
                None => locks.0.remove(&n),
            };
            match main {
                Some(lock) => world.insert_resource(lock),
                None => {
                    world.remove_resource::<InspectorLock>();
                }
            }
        }
        _ => draw(world),
    }
    match before {
        Some(before) => world.insert_resource(before),
        None => {
            world.remove_resource::<DrawingInstance>();
        }
    }
}

/// The Inspector lock of instance `n` (instance 0: the world's own lock).
#[must_use]
pub fn inspector_lock(world: &World, n: u8) -> Option<Entity> {
    if n == 0 {
        return world
            .get_resource::<crate::inspector::pinned::InspectorLock>()
            .and_then(|lock| lock.entity);
    }
    world
        .get_resource::<InspectorInstanceLocks>()
        .and_then(|locks| locks.0.get(&n).copied())
}

/// Sets the Inspector lock of instance `n`.
pub fn set_inspector_lock(world: &mut World, n: u8, entity: Option<Entity>) {
    if n == 0 {
        crate::inspector::pinned::set_lock(world, entity);
        return;
    }
    let mut locks = world.get_resource_or_insert_with(InspectorInstanceLocks::default);
    match entity {
        Some(entity) => locks.0.insert(n, entity),
        None => locks.0.remove(&n),
    };
}

/// Forgets everything instance `n` (`>= 1`) of `kind` kept in `world` (its tab closed).
pub fn drop_world_state(world: &mut World, kind: InstancedPanel, n: u8) {
    if n == 0 {
        return;
    }
    match kind {
        InstancedPanel::Outliner => {
            if let Some(mut caches) = world.get_resource_mut::<OutlinerCaches>() {
                caches.0.remove(&n);
            }
            let tree = Panel::Instance(kind, n).id();
            let has_tree = crate::outliner_columns::current_view_state(world)
                .is_some_and(|state| state.trees.contains_key(tree));
            if has_tree {
                crate::outliner_columns::edit_view_state(world, |state| {
                    state.trees.remove(tree);
                });
            }
        }
        InstancedPanel::Inspector => {
            if inspector_lock(world, n).is_some() {
                set_inspector_lock(world, n, None);
            }
        }
        InstancedPanel::Content => {}
    }
}

/// The saved form of an Inspector lock: `guid:<32 hex>` of the entity's guid (`None` without one).
#[must_use]
pub fn locked_text(world: &World, entity: Entity) -> Option<String> {
    let guid = world.get::<regia_scene::prelude::EntityGuid>(entity)?;
    Some(format!("guid:{:032x}", guid.0.0))
}

/// The entity a saved lock names in `world` (`None`: no such guid, or not a `guid:` text).
pub fn locked_entity(world: &mut World, text: &str) -> Option<Entity> {
    let guid: regia_core::id::Guid = text.strip_prefix("guid:")?.parse().ok()?;
    let mut query = world.query::<(Entity, &regia_scene::prelude::EntityGuid)>();
    query
        .iter(world)
        .find(|(_, g)| g.0 == guid)
        .map(|(entity, _)| entity)
}

/// The Content Browser instances `1..=7` and a pending restore, held by the shell (a browser's
/// state is the shell's, not the world's).
#[derive(Debug, Default)]
pub struct ShellInstances {
    /// Content Browser instances by number.
    pub content: BTreeMap<u8, ContentBrowserState>,
    /// States a restored layout brought, applied on the next frame (`None`: nothing pending).
    pub pending: Option<BTreeMap<String, PanelInstanceState>>,
}

/// The lowest instance number of `kind` free among `open` (`None`: all eight are open).
#[must_use]
pub fn free_instance(kind: InstancedPanel, open: &[Panel]) -> Option<u8> {
    (1..=MAX_PANEL_INSTANCE).find(|n| !open.contains(&Panel::Instance(kind, *n)))
}

/// The panels of the published layout, plus the instances already queued to open.
fn open_panels(world: &World) -> Vec<Panel> {
    let mut open: Vec<Panel> = world
        .get_resource::<LayoutSnapshot>()
        .map(|snapshot| {
            snapshot
                .doc
                .panel_ids()
                .into_iter()
                .chain(
                    snapshot
                        .doc
                        .floating
                        .iter()
                        .flat_map(|w| w.tabs.iter().map(String::as_str)),
                )
                .filter_map(Panel::from_id)
                .collect()
        })
        .unwrap_or_default();
    if let Some(requests) = world.get_resource::<LayoutRequests>() {
        open.extend(requests.pending.iter().filter_map(|request| match request {
            LayoutRequest::NewInstance(panel) => Some(*panel),
            _ => None,
        }));
    }
    open
}

/// Queues a new instance of `kind` for the shell's next frame; returns the instance's tab.
///
/// # Errors
///
/// `panel_instances_full` when all eight instances are open or queued.
pub fn queue_new_instance(world: &mut World, kind: InstancedPanel) -> Result<Panel, &'static str> {
    let n = free_instance(kind, &open_panels(world)).ok_or("panel_instances_full")?;
    let panel = Panel::Instance(kind, n);
    LayoutRequests::push(world, LayoutRequest::NewInstance(panel));
    Ok(panel)
}

/// The palette commands of this module (`editor.command { run }`). Returns the status line, `None`
/// for another id.
pub fn run_command(world: &mut World, id: &str) -> Option<String> {
    let (_, kind) = NEW_INSTANCE_COMMANDS.iter().find(|(command, _)| *command == id)?;
    Some(match queue_new_instance(world, *kind) {
        Ok(panel) => format!("Opening {}", panel.title()),
        Err(_) => format!(
            "{} instances of the {} are open (the most)",
            MAX_PANEL_INSTANCE + 1,
            kind.base().title()
        ),
    })
}

fn bad(code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

/// `editor.panel_new_instance { panel }`.
fn panel_new_instance(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = params.get("panel").and_then(Value::as_str).unwrap_or_default();
    let panel = Panel::from_id(id).ok_or_else(|| {
        bad(
            "unknown_panel",
            format!("no panel has the id {id:?}"),
            "outliner, content or inspector",
        )
    })?;
    let kind = InstancedPanel::of(panel)
        .filter(|_| panel.instance() == 0)
        .ok_or_else(|| {
            bad(
                "panel_not_instanced",
                format!("the {id} panel has no further instances"),
                "outliner, content or inspector (the plain id)",
            )
        })?;
    let panel = queue_new_instance(world, kind).map_err(|code| {
        bad(
            code,
            format!(
                "{} instances of the {} are open",
                MAX_PANEL_INSTANCE + 1,
                kind.base().title()
            ),
            "close one first",
        )
    })?;
    Ok(json!({
        "panel": panel.id(),
        "instance": panel.instance(),
        "title": panel.title(),
        "queued": true,
    }))
}

/// This module's agent methods, installed by `agent_ext::all_methods`.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.panel_new_instance",
            tool: "panel_new_instance",
            description: "Open a further instance of the Outliner, Content Browser or Inspector on the next frame (Window > New Outliner): the lowest free instance 1..=7, with its own search, columns, folder or lock. Its tab id is <panel>#<n>; editor.layout_get lists it and its saved state under instances.",
            params: &[ParamSchema {
                name: "panel",
                kind: "string",
                required: true,
                description: "outliner, content or inspector.",
            }],
            result: "{ panel, instance, title, queued }",
            mutating: true,
            permission: Permission::EditScene,
        },
        panel_new_instance as Handler,
    )]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_lowest_free_instance_is_taken_and_eight_is_the_most() {
        let kind = InstancedPanel::Outliner;
        assert_eq!(free_instance(kind, &[Panel::Outliner]), Some(1));
        let open = [Panel::Instance(kind, 1), Panel::Instance(kind, 3)];
        assert_eq!(free_instance(kind, &open), Some(2));
        // Another panel's instances do not count.
        let other = [Panel::Instance(InstancedPanel::Content, 1)];
        assert_eq!(free_instance(kind, &other), Some(1));
        let all: Vec<Panel> = (1..=MAX_PANEL_INSTANCE)
            .map(|n| Panel::Instance(kind, n))
            .collect();
        assert_eq!(free_instance(kind, &all), None);
    }

    #[test]
    fn queued_instances_count_as_open_and_the_ninth_is_refused() {
        let mut world = World::new();
        for n in 1..=MAX_PANEL_INSTANCE {
            let panel = queue_new_instance(&mut world, InstancedPanel::Inspector).unwrap();
            assert_eq!(panel, Panel::Instance(InstancedPanel::Inspector, n));
        }
        assert_eq!(
            queue_new_instance(&mut world, InstancedPanel::Inspector),
            Err("panel_instances_full")
        );
        let line = run_command(&mut world, "window.new_inspector").unwrap();
        assert!(line.contains("the most"), "{line}");
        assert!(
            run_command(&mut world, "window.new_outliner")
                .unwrap()
                .contains("Outliner 2")
        );
        assert_eq!(run_command(&mut world, "window.nothing"), None);
    }

    #[test]
    fn ids_are_scoped_only_for_instances_past_zero() {
        let mut world = World::new();
        assert_eq!(scoped_id(&world, "x"), egui::Id::new("x"));
        assert_eq!(outliner_tree(&world), crate::outliner_columns::MAIN_TREE);
        let panel = Panel::Instance(InstancedPanel::Outliner, 2);
        with_instance(&mut world, panel, |world| {
            assert_eq!(drawing_instance(world), 2);
            assert_eq!(scoped_id(world, "x"), egui::Id::new(("x", 2u8)));
            assert_eq!(outliner_tree(world), "outliner#2");
        });
        assert_eq!(drawing_instance(&world), 0, "the scope ends with the draw");
    }

    #[test]
    fn an_inspector_instance_keeps_its_own_lock() {
        use crate::inspector::pinned::InspectorLock;
        let mut world = World::new();
        let a = world.spawn_empty().id();
        let b = world.spawn_empty().id();
        set_inspector_lock(&mut world, 0, Some(a));
        let second = Panel::Instance(InstancedPanel::Inspector, 1);
        with_instance(&mut world, second, |world| {
            assert_eq!(
                world.resource::<InspectorLock>().entity,
                None,
                "instance 1 starts unlocked"
            );
            crate::inspector::pinned::set_lock(world, Some(b));
        });
        assert_eq!(inspector_lock(&world, 0), Some(a));
        assert_eq!(inspector_lock(&world, 1), Some(b));
        with_instance(&mut world, second, |world| {
            assert_eq!(world.resource::<InspectorLock>().entity, Some(b));
        });
        drop_world_state(&mut world, InstancedPanel::Inspector, 1);
        assert_eq!(inspector_lock(&world, 1), None);
        assert_eq!(inspector_lock(&world, 0), Some(a));
    }

    #[test]
    fn a_lock_saves_as_its_guid_and_restores_only_when_the_guid_is_there() {
        let mut world = World::new();
        let guid =
            regia_scene::prelude::EntityGuid(regia_core::id::Guid(0x1234_5678_9abc_def0_1122_3344_5566_7788));
        let entity = world.spawn(guid).id();
        let text = locked_text(&world, entity).unwrap();
        assert_eq!(text, "guid:123456789abcdef01122334455667788");
        assert_eq!(locked_entity(&mut world, &text), Some(entity));
        assert_eq!(
            locked_entity(&mut world, "guid:00000000000000000000000000000001"),
            None
        );
        assert_eq!(locked_entity(&mut world, "entity:5"), None);
        let bare = world.spawn_empty().id();
        assert_eq!(locked_text(&world, bare), None);
    }
}
