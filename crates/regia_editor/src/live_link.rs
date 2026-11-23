// SPDX-License-Identifier: MPL-2.0
//! Live link: edits made in the edit world are mirrored into a running session
//! (`docs/specs/19-editor.md` § 4, addendum § 15.5, ADR-7833, `book/agents/live-edit.md`).
//!
//! Off by default. While on, each editor frame (at most every [`MIN_INTERVAL`]) a pass reads the
//! edit world's ECS change ticks, as the level dirty feed does (ADR-7203): every component of a
//! level entity (one with an `EntityGuid`, not editor scaffolding) that changed since the last pass,
//! every entity added since then and every entity removed. The session's entity is found **by
//! guid** (the play copy and a launched game keep the level's guids), and the edit is made there:
//!
//! - a changed component (a property set, a transform drag) is written whole;
//! - a new entity is spawned with its components and its guid, under the session's copy of its
//!   parent (found by the parent's guid; a parent spawned in the same pass is spawned first and found
//!   through the mapping its spawn recorded); one the session already has (an undone delete) gets its
//!   components written instead;
//! - a removed entity is despawned with its children.
//!
//! The mirror is one-way: nothing flows back, and a session-side change is overwritten only when the
//! same component changes again in the edit world. An edit whose entity the session lacks (the game
//! despawned it, or it was never there) is not made and is listed as a **conflict** with its guid,
//! name and reason (`editor.live_link`), as is an edit the session refuses.
//!
//! The session is the runtime selected in the source switch (`crate::remote_world`), reached
//! through its agent server (`scene.query_entities`, `scene.get_entity`, `scene.set_property`,
//! `scene.spawn`, `scene.despawn`) on the connection's own thread (one pass in flight at a time; the
//! edits made meanwhile wait in the change ticks and go with the next pass), else the in-process
//! play world, written through reflection. When the session ends the link turns off.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::id::Guid;
use regia_ecs::ecs::change_detection::Tick;
use regia_ecs::prelude::*;
use regia_ecs::reflect::serde::{TypedReflectDeserializer, TypedReflectSerializer};
use regia_ecs::reflect::{AppTypeRegistry, ReflectComponent, TypeRegistry};
use regia_scene::components::EntityGuid;
use serde::de::DeserializeSeed as _;
use serde_json::{Map, Value, json};

use crate::remote_world::{JobKind, QueueError, RemoteTransport, RemoteWorld};
use crate::viewport_agent::Handler;

/// The shortest time between two pushes: a drag sends at most 20 updates a second, under the
/// runtime's default limit of 50 mutating calls a second.
pub const MIN_INTERVAL: Duration = Duration::from_millis(50);
/// Conflicts kept (the oldest are dropped).
pub const MAX_CONFLICTS: usize = 100;
/// The most entities one guid lookup in a runtime reads.
pub const LOOKUP_LIMIT: u64 = 10_000;

/// Components never mirrored: kept by the engine (transform propagation, visibility, hierarchy
/// relationships) or level bookkeeping.
pub const SKIPPED_COMPONENTS: &[&str] = &[
    "GlobalTransform",
    "InheritedVisibility",
    "ViewVisibility",
    "ChildOf",
    "Children",
    "EntityGuid",
    "LevelMember",
];

/// The protocol's error codes (lowercase ascii, listed on `book/agents/live-edit.md`).
pub mod codes {
    /// Live link was asked on with no play session and no runtime selected.
    pub const NO_SESSION: &str = "live_link_no_session";

    /// Every code, for the book page and its test.
    pub const ALL: &[&str] = &[NO_SESSION];
}

/// One edit the session did not take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The entity's guid.
    pub guid: Guid,
    /// Its name in the edit world.
    pub name: Option<String>,
    /// `set`, `spawn` or `despawn`.
    pub op: &'static str,
    /// The component, for `set`.
    pub component: Option<String>,
    /// Why: `missing in the session`, or the session's refusal.
    pub reason: String,
}

impl Conflict {
    fn to_json(&self) -> Value {
        json!({
            "guid": self.guid.to_string(),
            "name": self.name,
            "op": self.op,
            "component": self.component,
            "reason": self.reason,
        })
    }
}

/// One edit to mirror.
#[derive(Debug, Clone, PartialEq)]
pub enum MirrorOp {
    /// Write a whole component.
    Set {
        /// The entity.
        guid: Guid,
        /// Its edit-world name.
        name: Option<String>,
        /// The component's type path.
        component: String,
        /// The component's value (the reflect serializer's form).
        value: Value,
    },
    /// Spawn an entity with its components.
    Spawn {
        /// The entity.
        guid: Guid,
        /// Its edit-world name.
        name: Option<String>,
        /// Type path → value.
        components: Map<String, Value>,
        /// Its parent's guid (a level entity), when it has one.
        parent: Option<Guid>,
    },
    /// Despawn an entity and its children.
    Despawn {
        /// The entity.
        guid: Guid,
        /// Its edit-world name.
        name: Option<String>,
    },
}

/// The live link's switch and state (an edit-world resource).
#[derive(Resource, Debug, Default)]
pub struct LiveLink {
    /// Whether edits are mirrored.
    pub on: bool,
    /// Edits made in the session so far.
    pub pushed: u64,
    /// Edits the session did not take, newest last.
    pub conflicts: Vec<Conflict>,
    /// The session the last pass wrote to (`play` or a source id).
    pub target: Option<String>,
    last: Option<Tick>,
    known: BTreeMap<Guid, Option<String>>,
    sent: HashMap<(Guid, String), Value>,
    last_push: Option<Instant>,
    remote_ids: HashMap<Guid, u64>,
    /// The remote pass in flight: the connection's generation it was queued on.
    in_flight: Option<u64>,
    /// Edits a full queue did not take (sent with the next pass).
    backlog: Vec<MirrorOp>,
}

/// What a remote pass answered on the connection's thread.
struct LinkAnswer {
    ids: HashMap<Guid, u64>,
    conflicts: Vec<Conflict>,
    made: usize,
}

impl LiveLink {
    fn conflict(&mut self, conflict: Conflict) {
        if self.conflicts.contains(&conflict) {
            return;
        }
        self.conflicts.push(conflict);
        if self.conflicts.len() > MAX_CONFLICTS {
            let excess = self.conflicts.len() - MAX_CONFLICTS;
            self.conflicts.drain(..excess);
        }
    }
}

/// The edit world's level entities now: guid → (entity, name).
fn level_entities(world: &mut World) -> BTreeMap<Guid, (Entity, Option<String>)> {
    let editor_only = crate::editor_only::editor_only_guids(world);
    let found: Vec<(Entity, Guid)> = world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .filter(|(_, guid)| !editor_only.contains(&guid.0))
        .map(|(entity, guid)| (entity, guid.0))
        .collect();
    found
        .into_iter()
        .map(|(entity, guid)| (guid, (entity, name_of(world, entity))))
        .collect()
}

fn name_of(world: &World, entity: Entity) -> Option<String> {
    world
        .get::<regia_scene::components::Name>(entity)
        .map(|n| n.as_str().to_owned())
        .or_else(|| world.get::<Name>(entity).map(|n| n.as_str().to_owned()))
}

/// Turns the link on or off. On starts from the edit world as it is now: only later edits are
/// mirrored.
///
/// # Errors
///
/// [`codes::NO_SESSION`] when turning on with no play session and no runtime selected.
pub fn set_on(world: &mut World, on: bool) -> Result<(), RpcError> {
    if on && session_target(world).is_none() {
        return Err(RpcError::new(
            INVALID_PARAMS,
            codes::NO_SESSION,
            "live link needs a running session: start play, or select a launched game in the Outliner's source switch",
            "editor.play, or editor.remote_select { source }",
        ));
    }
    if !on {
        world.get_resource_or_insert_with(LiveLink::default).on = false;
        return Ok(());
    }
    let known = level_entities(world)
        .into_iter()
        .map(|(guid, (_, name))| (guid, name))
        .collect();
    let last = world.increment_change_tick();
    world.insert_resource(LiveLink {
        on: true,
        known,
        last: Some(last),
        ..LiveLink::default()
    });
    Ok(())
}

/// Where edits go now: a runtime source `(id, agent dir)`, `("play", None)` while play runs, or
/// `None`.
fn session_target(world: &World) -> Option<(String, Option<PathBuf>)> {
    let source = crate::remote_world::current(world);
    if let Some(dir) = source.dir {
        return Some((source.id, Some(dir)));
    }
    world
        .get_resource::<crate::pie::PieStatus>()
        .is_some_and(|s| s.mode.is_some())
        .then(|| (crate::remote_world::PLAY.to_owned(), None))
}

/// Whether `info`'s component is mirrored, and its reflection data when it is.
fn mirrored(
    registry: &TypeRegistry,
    type_id: Option<std::any::TypeId>,
) -> Option<(&'static str, ReflectComponent)> {
    let registration = registry.get(type_id?)?;
    let path = registration.type_info().type_path();
    let short = registration.type_info().type_path_table().short_path();
    if SKIPPED_COMPONENTS.contains(&short) || path.starts_with("regia_editor::") {
        return None;
    }
    Some((path, registration.data::<ReflectComponent>()?.clone()))
}

fn component_json(
    world: &World,
    registry: &TypeRegistry,
    entity: Entity,
    reflect: &ReflectComponent,
) -> Option<Value> {
    let value = reflect.reflect(world.get_entity(entity).ok()?)?;
    serde_json::to_value(TypedReflectSerializer::new(value.as_partial_reflect(), registry)).ok()
}

/// The edits made in the edit world since the last pass (see the module docs). Empty while the
/// link is off.
pub fn collect(world: &mut World) -> Vec<MirrorOp> {
    let Some(mut link) = world.remove_resource::<LiveLink>() else {
        return Vec::new();
    };
    if !link.on {
        world.insert_resource(link);
        return Vec::new();
    }
    let _span = tracing::debug_span!(target: "regia_editor", "live_link_collect").entered();
    let this = world.increment_change_tick();
    let last = link.last.unwrap_or(this);
    let registry = world
        .get_resource::<AppTypeRegistry>()
        .cloned()
        .unwrap_or_default();
    let registry = registry.read();
    let now = level_entities(world);
    let guid_of: HashMap<Entity, Guid> = now.iter().map(|(guid, (entity, _))| (*entity, *guid)).collect();
    let mut ops = Vec::new();
    for (guid, (entity, name)) in &now {
        let Ok(entity_ref) = world.get_entity(*entity) else {
            continue;
        };
        let added = !link.known.contains_key(guid);
        let mut components = Map::new();
        for id in entity_ref.archetype().components().iter() {
            if !added
                && !entity_ref
                    .get_change_ticks_by_id(*id)
                    .is_some_and(|ticks| ticks.is_changed(last, this))
            {
                continue;
            }
            let type_id = world.components().get_info(*id).and_then(|info| info.type_id());
            let Some((path, reflect)) = mirrored(&registry, type_id) else {
                continue;
            };
            let Some(value) = component_json(world, &registry, *entity, &reflect) else {
                continue;
            };
            let key = (*guid, path.to_owned());
            if !added && link.sent.get(&key) == Some(&value) {
                continue;
            }
            link.sent.insert(key, value.clone());
            if added {
                components.insert(path.to_owned(), value);
            } else {
                ops.push(MirrorOp::Set {
                    guid: *guid,
                    name: name.clone(),
                    component: path.to_owned(),
                    value,
                });
            }
        }
        if added {
            let parent = world
                .get::<ChildOf>(*entity)
                .and_then(|c| guid_of.get(&c.parent()).copied());
            ops.push(MirrorOp::Spawn {
                guid: *guid,
                name: name.clone(),
                components,
                parent,
            });
        }
    }
    for (guid, name) in &link.known {
        if !now.contains_key(guid) {
            link.sent.retain(|(g, _), _| g != guid);
            ops.push(MirrorOp::Despawn {
                guid: *guid,
                name: name.clone(),
            });
        }
    }
    drop(registry);
    link.known = now.into_iter().map(|(guid, (_, name))| (guid, name)).collect();
    link.last = Some(this);
    world.insert_resource(link);
    parents_first(ops)
}

/// `ops` with every spawn after the spawn of its parent (when the parent is spawned in the same
/// pass), so the parent's mirror mapping exists when the child is spawned. Other ops keep their
/// order.
fn parents_first(ops: Vec<MirrorOp>) -> Vec<MirrorOp> {
    let spawned: BTreeSet<Guid> = ops
        .iter()
        .filter_map(|op| match op {
            MirrorOp::Spawn { guid, .. } => Some(*guid),
            _ => None,
        })
        .collect();
    let mut placed: BTreeSet<Guid> = BTreeSet::new();
    let mut waiting = ops;
    let mut out = Vec::with_capacity(waiting.len());
    // Each round places every op whose parent is placed (or not spawned in this pass); a cycle
    // cannot happen in a hierarchy, but a round that places nothing ends the loop anyway.
    while !waiting.is_empty() {
        let before = waiting.len();
        let mut rest = Vec::new();
        for op in waiting {
            let ready = match &op {
                MirrorOp::Spawn {
                    parent: Some(parent), ..
                } => !spawned.contains(parent) || placed.contains(parent),
                _ => true,
            };
            if ready {
                if let MirrorOp::Spawn { guid, .. } = &op {
                    placed.insert(*guid);
                }
                out.push(op);
            } else {
                rest.push(op);
            }
        }
        if rest.len() == before {
            out.extend(rest);
            break;
        }
        waiting = rest;
    }
    out
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

/// A running session the link writes to. Entities are `Entity::to_bits` ids of the session.
pub trait SessionSink {
    /// The session's entity with `guid` (`name` narrows a runtime's search), `None` when it has none.
    ///
    /// # Errors
    ///
    /// The session's error text.
    fn resolve(&mut self, guid: Guid, name: Option<&str>) -> Result<Option<u64>, String>;
    /// Writes component `component` (a type path) of `entity`.
    ///
    /// # Errors
    ///
    /// The session's error text.
    fn set(&mut self, entity: u64, component: &str, value: &Value) -> Result<(), String>;
    /// Spawns an entity with `components` and `guid` (under session entity `parent`, when given);
    /// answers its id.
    ///
    /// # Errors
    ///
    /// The session's error text.
    fn spawn(
        &mut self,
        guid: Guid,
        name: Option<&str>,
        components: &Map<String, Value>,
        parent: Option<u64>,
    ) -> Result<u64, String>;
    /// Despawns `entity` and its children.
    ///
    /// # Errors
    ///
    /// The session's error text.
    fn despawn(&mut self, entity: u64) -> Result<(), String>;
}

/// The in-process play world, written through reflection.
pub struct PlaySink<'w> {
    world: &'w mut World,
    ids: Option<HashMap<Guid, Entity>>,
}

impl<'w> PlaySink<'w> {
    /// A sink over `world`.
    pub fn new(world: &'w mut World) -> Self {
        Self { world, ids: None }
    }

    fn registry(&self) -> AppTypeRegistry {
        self.world
            .get_resource::<AppTypeRegistry>()
            .cloned()
            .unwrap_or_default()
    }

    fn insert(&mut self, entity: Entity, component: &str, value: &Value) -> Result<(), String> {
        let registry = self.registry();
        let registry = registry.read();
        let registration = registry
            .get_with_type_path(component)
            .ok_or_else(|| format!("{component} is not registered in the session"))?;
        let reflect = registration
            .data::<ReflectComponent>()
            .ok_or_else(|| format!("{component} is not a reflected component"))?
            .clone();
        let built = TypedReflectDeserializer::new(registration, &registry)
            .deserialize(value)
            .map_err(|e| format!("{component}: {e}"))?;
        let mut entity_mut = self
            .world
            .get_entity_mut(entity)
            .map_err(|_| "the entity is gone".to_owned())?;
        reflect.insert(&mut entity_mut, built.as_partial_reflect(), &registry);
        Ok(())
    }
}

fn entity_of(bits: u64) -> Result<Entity, String> {
    Entity::try_from_bits(bits).ok_or_else(|| format!("{bits} is not an entity id"))
}

impl SessionSink for PlaySink<'_> {
    fn resolve(&mut self, guid: Guid, _name: Option<&str>) -> Result<Option<u64>, String> {
        let world = &mut *self.world;
        let ids = self.ids.get_or_insert_with(|| {
            world
                .query::<(Entity, &EntityGuid)>()
                .iter(world)
                .map(|(entity, guid)| (guid.0, entity))
                .collect()
        });
        Ok(ids
            .get(&guid)
            .filter(|entity| world.get_entity(**entity).is_ok())
            .map(|entity| entity.to_bits()))
    }

    fn set(&mut self, entity: u64, component: &str, value: &Value) -> Result<(), String> {
        self.insert(entity_of(entity)?, component, value)
    }

    fn spawn(
        &mut self,
        guid: Guid,
        _name: Option<&str>,
        components: &Map<String, Value>,
        parent: Option<u64>,
    ) -> Result<u64, String> {
        let parent = parent
            .map(entity_of)
            .transpose()?
            .filter(|p| self.world.get_entity(*p).is_ok());
        let entity = self.world.spawn(EntityGuid(guid)).id();
        for (component, value) in components {
            if let Err(err) = self.insert(entity, component, value) {
                tracing::debug!(target: "regia_editor", "live link spawn: {err}");
            }
        }
        if let Some(parent) = parent {
            self.world.entity_mut(entity).insert(ChildOf(parent));
        }
        if let Some(ids) = self.ids.as_mut() {
            ids.insert(guid, entity);
        }
        Ok(entity.to_bits())
    }

    fn despawn(&mut self, entity: u64) -> Result<(), String> {
        let entity = entity_of(entity)?;
        regia_scene::hierarchy::despawn_tree(self.world, entity)
            .then_some(())
            .ok_or_else(|| "the entity is gone".to_owned())
    }
}

/// A runtime reached through its agent server (the source switch's connection; the live link
/// runs it on the connection's thread).
pub struct RemoteSink<'r> {
    transport: &'r mut dyn RemoteTransport,
    ids: &'r mut HashMap<Guid, u64>,
}

impl<'r> RemoteSink<'r> {
    /// A sink over `transport`, caching guid lookups in `ids`.
    pub fn new(transport: &'r mut dyn RemoteTransport, ids: &'r mut HashMap<Guid, u64>) -> Self {
        Self { transport, ids }
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.transport.call(method, params).map_err(|e| e.to_string())
    }
}

impl SessionSink for RemoteSink<'_> {
    fn resolve(&mut self, guid: Guid, name: Option<&str>) -> Result<Option<u64>, String> {
        if let Some(id) = self.ids.get(&guid) {
            return Ok(Some(*id));
        }
        let mut query = json!({ "component": "EntityGuid", "limit": LOOKUP_LIMIT });
        // A name narrows the search to a few rows (a `*` in it would be a pattern: search all).
        if let Some(name) = name.filter(|n| !n.contains('*'))
            && let Value::Object(map) = &mut query
        {
            map.insert("name".into(), json!(name));
        }
        let answer = self.call("scene.query_entities", query)?;
        let known: BTreeSet<u64> = self.ids.values().copied().collect();
        let candidates: Vec<u64> = answer["entities"]
            .as_array()
            .map(|rows| rows.iter().filter_map(|r| r["entity"].as_u64()).collect())
            .unwrap_or_default();
        for entity in candidates.into_iter().filter(|e| !known.contains(e)) {
            let answer = self.call(
                "scene.get_entity",
                json!({ "entity": entity, "components": ["EntityGuid"] }),
            )?;
            let found = answer["components"]["EntityGuid"]["value"]
                .as_str()
                .and_then(|text| text.parse::<Guid>().ok());
            if let Some(found) = found {
                self.ids.insert(found, entity);
                if found == guid {
                    return Ok(Some(entity));
                }
            }
        }
        Ok(None)
    }

    fn set(&mut self, entity: u64, component: &str, value: &Value) -> Result<(), String> {
        self.call(
            "scene.set_property",
            json!({ "entity": entity, "component": component, "value": value }),
        )
        .map(drop)
    }

    fn spawn(
        &mut self,
        guid: Guid,
        name: Option<&str>,
        components: &Map<String, Value>,
        parent: Option<u64>,
    ) -> Result<u64, String> {
        let mut components = components.clone();
        components.insert(
            "regia_scene::components::EntityGuid".into(),
            json!(guid.to_string()),
        );
        let answer = self.call(
            "scene.spawn",
            json!({ "name": name, "components": components, "parent": parent }),
        )?;
        let id = answer["entity"]
            .as_u64()
            .ok_or_else(|| "scene.spawn answered no entity".to_owned())?;
        self.ids.insert(guid, id);
        Ok(id)
    }

    fn despawn(&mut self, entity: u64) -> Result<(), String> {
        self.ids.retain(|_, id| *id != entity);
        self.call("scene.despawn", json!({ "entity": entity })).map(drop)
    }
}

/// Makes `ops` in `sink`; answers how many were made and records the others as conflicts.
pub fn apply(link: &mut LiveLink, ops: &[MirrorOp], sink: &mut dyn SessionSink) -> usize {
    let mut made = 0;
    for op in ops {
        let (guid, name, kind, component) = op_parts(op);
        let found = match sink.resolve(guid, name.as_deref()) {
            Ok(found) => found,
            Err(reason) => {
                link.conflict(Conflict {
                    guid,
                    name,
                    op: kind,
                    component,
                    reason,
                });
                continue;
            }
        };
        let result = match (op, found) {
            (MirrorOp::Set { component, value, .. }, Some(entity)) => sink.set(entity, component, value),
            (MirrorOp::Set { .. }, None) => Err("missing in the session".to_owned()),
            // The session has it already (an undone delete the game kept): write its components.
            (MirrorOp::Spawn { components, .. }, Some(entity)) => components
                .iter()
                .try_for_each(|(component, value)| sink.set(entity, component, value)),
            (
                MirrorOp::Spawn {
                    components, parent, ..
                },
                None,
            ) => {
                // The parent by its guid: the session's copy, or one this pass spawned before.
                let session_parent = match parent {
                    Some(parent) => match sink.resolve(*parent, None) {
                        Ok(Some(id)) => Some(id),
                        Ok(None) | Err(_) => {
                            link.conflict(Conflict {
                                guid,
                                name: name.clone(),
                                op: "spawn",
                                component: None,
                                reason: format!(
                                    "parent {parent} missing in the session: spawned at the root"
                                ),
                            });
                            None
                        }
                    },
                    None => None,
                };
                sink.spawn(guid, name.as_deref(), components, session_parent)
                    .map(drop)
            }
            (MirrorOp::Despawn { .. }, Some(entity)) => sink.despawn(entity),
            // Already gone there.
            (MirrorOp::Despawn { .. }, None) => Ok(()),
        };
        match result {
            Ok(()) => made += 1,
            Err(reason) => link.conflict(Conflict {
                guid,
                name,
                op: kind,
                component,
                reason,
            }),
        }
    }
    link.pushed += made as u64;
    made
}

/// One pass: the edits since the last pass, made in `sink`. Answers how many were made.
pub fn pass(world: &mut World, sink: &mut dyn SessionSink) -> usize {
    let ops = collect(world);
    if ops.is_empty() {
        return 0;
    }
    let mut link = world.remove_resource::<LiveLink>().unwrap_or_default();
    let made = apply(&mut link, &ops, sink);
    world.insert_resource(link);
    made
}

/// The editor's per-frame entry: with the link on, at most every [`MIN_INTERVAL`], mirrors the edit
/// world's edits into the selected runtime or `play`. The link turns off when its session is gone.
pub fn pump(edit: &mut World, play: Option<&mut World>) {
    let Some(link) = edit.get_resource::<LiveLink>() else {
        return;
    };
    if !link.on {
        return;
    }
    let recent = link.last_push.is_some_and(|at| at.elapsed() < MIN_INTERVAL);
    // The session check comes before the rate gate: a link whose session ended turns off at once.
    let target = session_target(edit);
    let playing = play.is_some();
    let Some((id, dir)) = target.filter(|(_, dir)| dir.is_some() || playing) else {
        let mut link = edit.resource_mut::<LiveLink>();
        link.on = false;
        tracing::info!(target: "regia_editor", "live link off: the session ended");
        return;
    };
    if recent {
        return;
    }
    let _span = tracing::debug_span!(target: "regia_editor", "live_link_pump").entered();
    {
        let mut link = edit.resource_mut::<LiveLink>();
        link.last_push = Some(Instant::now());
        if link.target.as_deref() != Some(id.as_str()) {
            link.target = Some(id);
            link.remote_ids.clear();
            link.in_flight = None;
            link.backlog.clear();
        }
    }
    match (dir, play) {
        (Some(dir), _) => remote_pass(edit, &dir),
        (None, Some(play)) => {
            pass(edit, &mut PlaySink::new(play));
        }
        (None, None) => {}
    }
}

/// Takes the answers of earlier remote passes into `link`; a pass whose connection was replaced is
/// forgotten.
fn take_answers(link: &mut LiveLink, remote: &mut RemoteWorld) {
    for answer in remote.take_link_answers() {
        let Ok(answer) = answer.downcast::<LinkAnswer>() else {
            continue;
        };
        link.remote_ids = answer.ids;
        for conflict in answer.conflicts {
            link.conflict(conflict);
        }
        link.pushed += answer.made as u64;
        link.in_flight = None;
    }
    if link
        .in_flight
        .is_some_and(|generation| generation != remote.generation())
    {
        link.in_flight = None;
    }
}

/// One pass into the runtime at agent directory `dir`, queued on the connection's thread: at most
/// one in flight; the edits made meanwhile are collected by the next pass.
fn remote_pass(edit: &mut World, dir: &Path) {
    let mut link = edit.remove_resource::<LiveLink>().unwrap_or_default();
    let mut remote = edit.remove_resource::<RemoteWorld>().unwrap_or_default();
    take_answers(&mut link, &mut remote);
    if link.in_flight.is_none() {
        edit.insert_resource(link);
        let ops = collect(edit);
        link = edit.remove_resource::<LiveLink>().unwrap_or_default();
        let mut ops_all = std::mem::take(&mut link.backlog);
        ops_all.extend(ops);
        if !ops_all.is_empty() {
            let ids = std::mem::take(&mut link.remote_ids);
            let ops = ops_all.clone();
            let job: crate::remote_world::Job = Box::new(
                move |transport: &mut dyn RemoteTransport| -> crate::remote_world::Answer {
                    let mut ids = ids;
                    let mut scratch = LiveLink::default();
                    let made = apply(&mut scratch, &ops, &mut RemoteSink::new(transport, &mut ids));
                    Box::new(LinkAnswer {
                        ids,
                        conflicts: scratch.conflicts,
                        made,
                    })
                },
            );
            match remote.try_queue(dir, JobKind::Link, job) {
                Ok(_) => link.in_flight = Some(remote.generation()),
                // The queue is full: the edits go with the next pass.
                Err(QueueError::Full) => link.backlog = ops_all,
                Err(QueueError::Closed(err)) => {
                    for op in &ops_all {
                        let (guid, name, kind, component) = op_parts(op);
                        link.conflict(Conflict {
                            guid,
                            name,
                            op: kind,
                            component,
                            reason: err.to_string(),
                        });
                    }
                }
            }
        }
    }
    edit.insert_resource(remote);
    edit.insert_resource(link);
}

/// Waits until the remote pass in flight (if any) has come back and takes its answer (tests and
/// the agent's status read).
pub fn flush(edit: &mut World) {
    let Some(mut link) = edit.remove_resource::<LiveLink>() else {
        return;
    };
    let mut remote = edit.remove_resource::<RemoteWorld>().unwrap_or_default();
    take_answers(&mut link, &mut remote);
    while link.in_flight.is_some() {
        if !remote.wait_any() {
            link.in_flight = None;
            break;
        }
        take_answers(&mut link, &mut remote);
    }
    edit.insert_resource(remote);
    edit.insert_resource(link);
}

/// An op's guid, name, kind and component.
fn op_parts(op: &MirrorOp) -> (Guid, Option<String>, &'static str, Option<String>) {
    match op {
        MirrorOp::Set {
            guid,
            name,
            component,
            ..
        } => (*guid, name.clone(), "set", Some(component.clone())),
        MirrorOp::Spawn { guid, name, .. } => (*guid, name.clone(), "spawn", None),
        MirrorOp::Despawn { guid, name } => (*guid, name.clone(), "despawn", None),
    }
}

// ---------------------------------------------------------------------------
// Panel
// ---------------------------------------------------------------------------

/// The Live link toggle and its conflict count, beside the source switch. Drawn only while a
/// session is running (or the link is on).
pub fn toggle_ui(ui: &mut egui::Ui, world: &mut World) {
    let session = session_target(world).is_some();
    let link = world.get_resource::<LiveLink>();
    let mut on = link.is_some_and(|l| l.on);
    if !session && !on {
        return;
    }
    let conflicts = link.map_or(0, |l| l.conflicts.len());
    let response = crate::agent_parity::action_toggle(ui, &crate::agent_parity::LIVE_LINK, &mut on)
        .on_hover_text(
            "Mirror edits made in the edit world into the running session by entity guid (one-way).",
        );
    if response.changed() {
        // A frame may run the UI twice; the second pass sees the switch already set.
        let already = world.get_resource::<LiveLink>().is_some_and(|l| l.on) == on;
        if !already && let Err(err) = set_on(world, on) {
            tracing::warn!(target: "regia_editor", "live link: {}", err.message);
        }
    }
    if conflicts > 0 {
        let warn = crate::theme::tokens(world).warn;
        let lines: Vec<String> = world
            .get_resource::<LiveLink>()
            .map(|l| {
                l.conflicts
                    .iter()
                    .rev()
                    .take(10)
                    .map(|c| {
                        format!(
                            "{} {} ({}): {}",
                            c.op,
                            c.name.clone().unwrap_or_else(|| c.guid.to_string()),
                            c.component.as_deref().unwrap_or("entity"),
                            c.reason
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        ui.colored_label(warn, format!("{conflicts} not mirrored"))
            .on_hover_text(lines.join("\n"));
    }
}

// ---------------------------------------------------------------------------
// Agent method
// ---------------------------------------------------------------------------

fn status_json(world: &World) -> Value {
    let link = world.get_resource::<LiveLink>();
    json!({
        "on": link.is_some_and(|l| l.on),
        "session": session_target(world).map(|(id, _)| id),
        "target": link.and_then(|l| l.target.clone()),
        "pushed": link.map_or(0, |l| l.pushed),
        "conflicts": link.map_or_else(Vec::new, |l| l.conflicts.iter().map(Conflict::to_json).collect()),
    })
}

/// `editor.live_link { on? }`.
fn editor_live_link(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // A remote pass in flight is waited for, so `pushed` and `conflicts` are current.
    flush(world);
    match params.get("on") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(on)) => set_on(world, *on)?,
        Some(_) => return Err(RpcError::wrong_type("on", "a boolean")),
    }
    Ok(status_json(world))
}

/// The live link's method.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.live_link",
            tool: "live_link",
            description: "Live link (off by default): edits made in the edit world (property sets, transform \
                drags, spawns and deletes of level entities) are mirrored one-way into the running session by \
                entity guid: the runtime selected with editor.remote_select, else the play world. Edits whose \
                entity the session lacks are listed in `conflicts`. Without `on` it answers the status. \
                live_link_no_session.",
            params: &[ParamSchema {
                name: "on",
                kind: "boolean",
                required: false,
                description: "Turn the link on or off (on starts from the edit world as it is now).",
            }],
            result: "{ on, session, target, pushed, conflicts: [{ guid, name, op, component, reason }] }",
            mutating: true,
            permission: Permission::Play,
        },
        editor_live_link as Handler,
    )]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use regia_app::prelude::*;
    use regia_core::math::Vec3;
    use regia_scene::prelude::Transform;

    const A: u128 = 0x7833_0000_0000_4000_8000_0000_0000_000a;
    const B: u128 = 0x7833_0000_0000_4000_8000_0000_0000_000b;

    fn scene_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.update();
        app
    }

    fn spawn(world: &mut World, guid: u128, name: &str, x: f32) -> Entity {
        world
            .spawn((
                EntityGuid(Guid::from_u128(guid)),
                regia_scene::components::Name::new(name),
                Transform::from_translation(Vec3::new(x, 0.0, 0.0)),
            ))
            .id()
    }

    fn by_guid(world: &mut World, guid: u128) -> Option<Entity> {
        world
            .query::<(Entity, &EntityGuid)>()
            .iter(world)
            .find(|(_, g)| g.0 == Guid::from_u128(guid))
            .map(|(e, _)| e)
    }

    /// The mapping test: edits reach the session's entity with the same guid, whatever its id.
    #[test]
    fn edits_map_to_the_session_entity_by_guid() {
        let mut edit = scene_app();
        let mut play = scene_app();
        // Different spawn order: the ids differ, the guids match.
        let a_edit = spawn(edit.world_mut(), A, "Crate", 0.0);
        spawn(edit.world_mut(), B, "Lamp", 1.0);
        spawn(play.world_mut(), B, "Lamp", 1.0);
        let a_play = spawn(play.world_mut(), A, "Crate", 0.0);
        assert_ne!(a_edit.to_bits(), a_play.to_bits(), "the test needs different ids");
        set_on(edit.world_mut(), true).expect_err("no session yet");
        edit.world_mut().insert_resource(crate::pie::PieStatus {
            mode: Some(crate::pie::PlayMode::Playing),
            ..Default::default()
        });
        set_on(edit.world_mut(), true).expect("on during play");

        // Nothing changed yet: nothing is sent.
        assert_eq!(pass(edit.world_mut(), &mut PlaySink::new(play.world_mut())), 0);
        edit.world_mut()
            .get_mut::<Transform>(a_edit)
            .expect("transform")
            .translation
            .y = 3.0;
        assert_eq!(pass(edit.world_mut(), &mut PlaySink::new(play.world_mut())), 1);
        let moved = play
            .world()
            .get::<Transform>(a_play)
            .expect("transform")
            .translation;
        assert_eq!(moved, Vec3::new(0.0, 3.0, 0.0));
        let lamp = by_guid(play.world_mut(), B).expect("lamp");
        assert_eq!(
            play.world()
                .get::<Transform>(lamp)
                .expect("transform")
                .translation,
            Vec3::new(1.0, 0.0, 0.0),
            "the other entity is untouched"
        );
    }

    #[test]
    fn spawns_deletes_and_missing_entities() {
        let mut edit = scene_app();
        let mut play = scene_app();
        let lamp = spawn(edit.world_mut(), B, "Lamp", 1.0);
        edit.world_mut().insert_resource(crate::pie::PieStatus {
            mode: Some(crate::pie::PlayMode::Playing),
            ..Default::default()
        });
        set_on(edit.world_mut(), true).expect("on");
        // The game despawned the lamp (it never had it): an edit to it is a conflict.
        edit.world_mut()
            .get_mut::<Transform>(lamp)
            .expect("t")
            .translation
            .z = 2.0;
        // A new entity is spawned in the session with its guid.
        spawn(edit.world_mut(), A, "Crate", 5.0);
        let made = pass(edit.world_mut(), &mut PlaySink::new(play.world_mut()));
        assert_eq!(made, 1, "the spawn");
        let crate_play = by_guid(play.world_mut(), A).expect("spawned by guid");
        assert_eq!(
            play.world()
                .get::<Transform>(crate_play)
                .expect("t")
                .translation
                .x,
            5.0
        );
        let link = edit.world().resource::<LiveLink>();
        assert_eq!(link.conflicts.len(), 1, "{:?}", link.conflicts);
        assert_eq!(link.conflicts[0].guid, Guid::from_u128(B));
        assert_eq!(link.conflicts[0].reason, "missing in the session");
        // Deleting in the edit world deletes in the session.
        let crate_edit = by_guid(edit.world_mut(), A).expect("crate");
        edit.world_mut().despawn(crate_edit);
        assert_eq!(pass(edit.world_mut(), &mut PlaySink::new(play.world_mut())), 1);
        assert!(by_guid(play.world_mut(), A).is_none(), "despawned in the session");
    }

    #[test]
    fn off_by_default_and_editor_only_skipped() {
        let mut edit = scene_app();
        let mut play = scene_app();
        let a = spawn(edit.world_mut(), A, "Crate", 0.0);
        spawn(play.world_mut(), A, "Crate", 0.0);
        edit.world_mut().get_mut::<Transform>(a).expect("t").translation.x = 4.0;
        assert_eq!(
            pass(edit.world_mut(), &mut PlaySink::new(play.world_mut())),
            0,
            "off"
        );
        edit.world_mut().insert_resource(crate::pie::PieStatus {
            mode: Some(crate::pie::PlayMode::Playing),
            ..Default::default()
        });
        set_on(edit.world_mut(), true).expect("on");
        edit.world_mut()
            .spawn((EntityGuid(Guid::from_u128(B)), crate::editor_only::EditorOnly));
        assert_eq!(
            pass(edit.world_mut(), &mut PlaySink::new(play.world_mut())),
            0,
            "scaffolding stays"
        );
    }

    /// The edit world the editor builds (scene, editor resources, an open level), as `tests/pie.rs`.
    pub(crate) fn edit_app() -> App {
        let mut app = scene_app();
        app.init_resource::<crate::command::UndoStack>()
            .init_resource::<crate::selection::Selection>()
            .init_resource::<crate::viewport::EditorCamera>()
            .init_resource::<crate::viewport::GizmoState>()
            .init_resource::<crate::viewport::ViewportSettings>()
            .init_resource::<crate::pie::PieRequests>()
            .init_resource::<crate::pie::PieStatus>();
        let open = crate::level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        app.update();
        app
    }

    /// Acceptance (P-303): with Live link on during play, a transform drag in the edit world (the
    /// gizmo's `SetTransform` command) moves the play-world entity with the same guid.
    #[test]
    fn a_transform_drag_moves_the_play_entity_with_the_same_guid() {
        use crate::command::{EditorCommand as _, SetTransform, SpawnEntities};
        let mut edit = edit_app();
        let guid = Guid::from_u128(A);
        let mut spawn =
            SpawnEntities::new_entity(edit.world(), guid, "Crate", Transform::default()).expect("spawn");
        spawn.apply(edit.world_mut());
        let entity = by_guid(edit.world_mut(), A).expect("spawned");
        let mut session = crate::pie::start_play(edit.world_mut(), None).expect("play starts");
        crate::pie::sync_status(edit.world_mut(), Some(&session));
        let status = crate::remote_world::tests::call_method(edit.world_mut(), "editor.live_link", json!({}))
            .expect("status");
        assert_eq!(status["on"], json!(false), "off by default: {status}");
        let status = crate::remote_world::tests::call_method(
            edit.world_mut(),
            "editor.live_link",
            json!({ "on": true }),
        )
        .expect("on");
        assert_eq!(status["session"], json!("play"), "{status}");

        let target = Transform::from_translation(Vec3::new(4.0, 0.0, -2.0));
        let mut drag = SetTransform::capture(edit.world(), entity, target);
        drag.apply(edit.world_mut());
        pump(edit.world_mut(), Some(session.app.world_mut()));

        let played = by_guid(session.app.world_mut(), A).expect("the play copy keeps the guid");
        assert_eq!(
            session
                .app
                .world()
                .get::<Transform>(played)
                .expect("t")
                .translation,
            Vec3::new(4.0, 0.0, -2.0)
        );
        let status = crate::remote_world::tests::call_method(edit.world_mut(), "editor.live_link", json!({}))
            .expect("status");
        assert_eq!(status["pushed"], json!(1), "{status}");
        assert_eq!(status["conflicts"], json!([]), "{status}");
        crate::pie::stop_play(session);
        crate::pie::sync_status(edit.world_mut(), None);
        // The session is gone: the link turns itself off.
        pump(edit.world_mut(), None);
        assert!(!edit.world().resource::<LiveLink>().on);
    }

    /// The live link into a launched runtime: a drag in the edit world moves the runtime's entity
    /// with the same guid, reached through its agent server.
    #[test]
    fn a_drag_moves_the_launched_runtime_entity_by_guid() {
        use crate::remote_world::tests::{CRATE_GUID, HeadlessRuntime};
        let runtime = HeadlessRuntime::start("live-link");
        let mut edit = scene_app();
        let entity = spawn(edit.world_mut(), CRATE_GUID, "Crate", 1.0);
        let id = crate::remote_world::attach(edit.world_mut(), "Headless game", runtime.dir.clone());
        crate::remote_world::select_source(edit.world_mut(), &id).expect("source");
        set_on(edit.world_mut(), true).expect("a runtime is a session");
        edit.world_mut()
            .get_mut::<Transform>(entity)
            .expect("t")
            .translation = Vec3::new(1.0, 7.0, 0.0);
        pump(edit.world_mut(), None);
        // The pass runs on the connection's thread: wait for its answer.
        flush(edit.world_mut());
        let link = edit.world().resource::<LiveLink>();
        assert_eq!(link.conflicts, Vec::new());
        assert_eq!(link.pushed, 1);
        assert_eq!(link.target.as_deref(), Some(id.as_str()));
        assert_eq!(runtime.finish(), Some([1.0, 7.0, 0.0]));
    }

    /// The session entity `child`'s parent, as an `Entity`.
    fn parent_of(world: &World, child: Entity) -> Option<Entity> {
        world.get::<ChildOf>(child).map(|c| c.parent())
    }

    /// Acceptance (P-303 item 3): a child spawned in the edit world is spawned in the session under
    /// the session's copy of its parent (found by the parent's guid), including a parent spawned in
    /// the same pass (the parent's mirror mapping).
    #[test]
    fn a_mirrored_child_spawn_keeps_its_parent() {
        const CHILD: u128 = 0x7833_0000_0000_4000_8000_0000_0000_00c1;
        const NEW_PARENT: u128 = 0x7833_0000_0000_4000_8000_0000_0000_00b2;
        const NEW_CHILD: u128 = 0x7833_0000_0000_4000_8000_0000_0000_0091;
        let mut edit = scene_app();
        let mut play = scene_app();
        // Different spawn order: the parent's ids differ, its guid matches.
        spawn(play.world_mut(), B, "Lamp", 1.0);
        let parent_play = spawn(play.world_mut(), A, "Cart", 0.0);
        let parent_edit = spawn(edit.world_mut(), A, "Cart", 0.0);
        assert_ne!(
            parent_edit.to_bits(),
            parent_play.to_bits(),
            "the test needs different ids"
        );
        edit.world_mut().insert_resource(crate::pie::PieStatus {
            mode: Some(crate::pie::PlayMode::Playing),
            ..Default::default()
        });
        set_on(edit.world_mut(), true).expect("on");

        // A child under an entity the session has.
        let child = spawn(edit.world_mut(), CHILD, "Wheel", 0.5);
        edit.world_mut().entity_mut(child).insert(ChildOf(parent_edit));
        // A new parent and its new child in the same pass (the child's guid sorts first).
        let new_parent = spawn(edit.world_mut(), NEW_PARENT, "Trailer", 3.0);
        let new_child = spawn(edit.world_mut(), NEW_CHILD, "Hitch", 0.1);
        edit.world_mut().entity_mut(new_child).insert(ChildOf(new_parent));
        assert!(
            Guid::from_u128(NEW_CHILD) < Guid::from_u128(NEW_PARENT),
            "the new child sorts before its new parent"
        );
        let made = pass(edit.world_mut(), &mut PlaySink::new(play.world_mut()));
        assert_eq!(made, 3, "three spawns");
        let link = edit.world().resource::<LiveLink>();
        assert_eq!(link.conflicts, Vec::new());

        let child_play = by_guid(play.world_mut(), CHILD).expect("the child is spawned");
        assert_eq!(
            parent_of(play.world(), child_play),
            Some(parent_play),
            "the child keeps its parent (the session's entity with the parent's guid)"
        );
        let new_parent_play = by_guid(play.world_mut(), NEW_PARENT).expect("new parent");
        let new_child_play = by_guid(play.world_mut(), NEW_CHILD).expect("new child");
        assert_eq!(
            parent_of(play.world(), new_child_play),
            Some(new_parent_play),
            "a parent spawned in the same pass is found through its mirror mapping"
        );
        assert_eq!(
            parent_of(play.world(), new_parent_play),
            None,
            "a root stays a root"
        );
    }

    /// P-303 item 3 through a launched runtime: a child spawned in the edit world under the Crate is
    /// spawned in the runtime under the runtime's Crate (found by guid through its agent server).
    #[test]
    fn a_mirrored_child_spawn_keeps_its_parent_in_a_launched_game() {
        use crate::remote_world::tests::{CRATE_GUID, HeadlessRuntime};
        const WHEEL: u128 = 0x7833_0000_0000_4000_8000_0000_0000_00c3;
        let runtime = HeadlessRuntime::start("live-link-child");
        let mut edit = scene_app();
        let crate_edit = spawn(edit.world_mut(), CRATE_GUID, "Crate", 1.0);
        let id = crate::remote_world::attach(edit.world_mut(), "Headless game", runtime.dir.clone());
        crate::remote_world::select_source(edit.world_mut(), &id).expect("source");
        set_on(edit.world_mut(), true).expect("a runtime is a session");
        let wheel = spawn(edit.world_mut(), WHEEL, "Wheel", 0.5);
        edit.world_mut().entity_mut(wheel).insert(ChildOf(crate_edit));
        pump(edit.world_mut(), None);
        flush(edit.world_mut());
        let link = edit.world().resource::<LiveLink>();
        assert_eq!(link.conflicts, Vec::new());
        assert_eq!(link.pushed, 1);
        let dir = runtime.dir.clone();
        let mut remote = edit.world_mut().remove_resource::<RemoteWorld>().expect("remote");
        let mut find = |name: &str| {
            remote
                .call_runtime(&dir, "scene.query_entities", json!({ "name": name }))
                .expect("query")["entities"][0]["entity"]
                .as_u64()
                .unwrap_or_else(|| panic!("{name} in the runtime"))
        };
        let (crate_rt, wheel_rt) = (find("Crate"), find("Wheel"));
        let answer = remote
            .call_runtime(&dir, "scene.get_entity", json!({ "entity": wheel_rt }))
            .expect("wheel");
        assert_eq!(
            answer["parent"],
            json!(crate_rt),
            "the runtime's Wheel is under its Crate: {answer}"
        );
        drop(remote);
        runtime.finish();
    }

    #[test]
    fn codes_are_lowercase_ascii() {
        for code in codes::ALL.iter().chain(crate::remote_world::codes::ALL) {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
        }
    }
}
