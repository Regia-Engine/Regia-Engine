// SPDX-License-Identifier: MPL-2.0
//! The `SmartObject::definition` reference (spec 39 § 4, ADR-2012): a scene names a definition by
//! [`AssetId`]; this module resolves the id through the asset server, reads the asset (its
//! `.smartobject.ron` text when there is one, else the cooked pak entry) and sets
//! [`SmartObjectDefinition`], which materializes the slots.
//!
//! A definition is read once per id however many objects use it, so they share one `Arc`. A
//! changed source file is read again; a file that does not load keeps the definition in use and
//! reports the error. A host that parses a definition itself and sets [`SmartObjectDefinition`] on
//! an object with no `definition` is not touched.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset::{AssetId, AssetServer, SourceAssetChanged};
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_framework::table_source::{TableBytes, read_table};

use crate::smart_object::SmartObject;
use crate::smart_object_def::{SMART_OBJECT_DEF_TYPE_PATH, SmartObjectDef, SmartObjectDefinition};

/// Why a definition is not in use (`reason` is the agent's `definition_error.reason`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionError {
    /// `definition_not_found`, `bad_definition` or `unsupported_version`.
    pub reason: &'static str,
    /// Human-readable detail.
    pub detail: String,
}

/// One definition id: what loaded, from where, and the last failure.
#[derive(Debug, Clone, Default)]
pub struct DefinitionSlot {
    /// The definition in use (kept when a reload fails).
    pub definition: Option<Arc<SmartObjectDef>>,
    /// Asset path under `assets/`, once the id resolved.
    pub path: Option<String>,
    /// Whether the definition came from a cooked entry rather than source text.
    pub cooked: bool,
    /// Bumped on every applied load.
    pub generation: u64,
    /// The last failure, cleared by a load that works.
    pub error: Option<DefinitionError>,
}

/// Definitions loaded for [`SmartObject::definition`], by id.
#[derive(Resource, Debug, Default)]
pub struct SmartObjectDefs {
    slots: BTreeMap<AssetId, DefinitionSlot>,
}

impl SmartObjectDefs {
    /// The slot of `id`.
    #[must_use]
    pub fn slot(&self, id: AssetId) -> Option<&DefinitionSlot> {
        self.slots.get(&id)
    }

    /// Every slot, sorted by id.
    pub fn iter(&self) -> impl Iterator<Item = (AssetId, &DefinitionSlot)> {
        self.slots.iter().map(|(id, slot)| (*id, slot))
    }

    /// Puts an already parsed `definition` at `id` and returns its generation.
    pub fn insert(&mut self, id: AssetId, definition: SmartObjectDef) -> u64 {
        let slot = self.slots.entry(id).or_default();
        slot.definition = Some(Arc::new(definition));
        slot.generation += 1;
        slot.error = None;
        slot.generation
    }

    fn fail(&mut self, id: AssetId, reason: &'static str, detail: String) {
        tracing::warn!(target: "regia_ai", %id, reason, "smart object definition not loaded: {detail}");
        self.slots.entry(id).or_default().error = Some(DefinitionError { reason, detail });
    }
}

/// Reads the definition `id` into [`SmartObjectDefs`] and returns whether one is in use. A failed
/// read keeps what was there and records why in [`DefinitionSlot::error`].
pub fn load_definition(world: &mut World, id: AssetId) -> bool {
    let _span = tracing::debug_span!(target: "regia_ai", "smart_object_definition_load").entered();
    if !world.contains_resource::<SmartObjectDefs>() {
        world.init_resource::<SmartObjectDefs>();
    }
    let asset = world
        .get_resource::<AssetServer>()
        .and_then(|server| server.path_for_id(id))
        .map(|path| path.strip_prefix("assets/").unwrap_or(path).to_owned());
    let read = match &asset {
        Some(path) => read_table(world, path, SMART_OBJECT_DEF_TYPE_PATH),
        None => Err("no cooked manifest names this id (run `regia cook`)".to_owned()),
    };
    let mut defs = world.resource_mut::<SmartObjectDefs>();
    if asset.is_some() {
        defs.slots.entry(id).or_default().path.clone_from(&asset);
    }
    let (text, cooked) = match read {
        Ok(TableBytes::Text(text)) => (text, false),
        Ok(TableBytes::Cooked(bytes)) => match String::from_utf8(bytes) {
            Ok(text) => (text, true),
            Err(_) => {
                defs.fail(id, "bad_definition", "the cooked entry is not UTF-8".to_owned());
                return defs.slot(id).is_some_and(|slot| slot.definition.is_some());
            }
        },
        Err(detail) => {
            defs.fail(id, "definition_not_found", detail);
            return defs.slot(id).is_some_and(|slot| slot.definition.is_some());
        }
    };
    match SmartObjectDef::from_ron(&text) {
        Ok(definition) => {
            defs.insert(id, definition);
            if let Some(slot) = defs.slots.get_mut(&id) {
                slot.cooked = cooked;
            }
            true
        }
        Err(error) => {
            defs.fail(id, error.reason(), error.to_string());
            defs.slot(id).is_some_and(|slot| slot.definition.is_some())
        }
    }
}

/// The ids to read again for one changed `file`: the id at that path, and every id that has no
/// definition yet (the file may be what it was waiting for).
fn ids_for_change(defs: &SmartObjectDefs, file: &Path, out: &mut Vec<AssetId>) {
    for (id, slot) in defs.iter() {
        let matches = slot
            .path
            .as_deref()
            .is_some_and(|path| file.ends_with(Path::new(path)));
        if (matches || slot.definition.is_none()) && !out.contains(&id) {
            out.push(id);
        }
    }
}

/// `PostUpdate`, before `smart_object_materialize`: reads the definitions objects name and sets
/// [`SmartObjectDefinition`] where it is missing or out of date. Takes no allocation when nothing
/// changed.
#[tracing::instrument(skip_all, target = "regia_ai", name = "smart_object_load_definitions")]
pub(crate) fn smart_object_load_definitions(
    world: &mut World,
    query: &mut QueryState<(Entity, &SmartObject, Option<&SmartObjectDefinition>)>,
    mut changes: Local<MessageCursor<SourceAssetChanged>>,
    mut todo: Local<Vec<(Entity, AssetId)>>,
) {
    let mut reload: Vec<AssetId> = Vec::new();
    if let (Some(messages), Some(defs)) = (
        world.get_resource::<Messages<SourceAssetChanged>>(),
        world.get_resource::<SmartObjectDefs>(),
    ) {
        for change in changes.read(messages) {
            let is_definition = change
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.to_ascii_lowercase().ends_with(".smartobject.ron"));
            if is_definition {
                ids_for_change(defs, &change.path, &mut reload);
            }
        }
    }
    for id in reload {
        load_definition(world, id);
    }
    todo.clear();
    let defs = world.get_resource::<SmartObjectDefs>();
    for (entity, object, current) in query.iter(world) {
        let Some(reference) = &object.definition else {
            continue;
        };
        let wanted = match defs.and_then(|defs| defs.slot(reference.id)) {
            None => true,
            Some(slot) => slot
                .definition
                .as_ref()
                .is_some_and(|def| current.is_none_or(|c| !Arc::ptr_eq(&c.0, def))),
        };
        if wanted {
            todo.push((entity, reference.id));
        }
    }
    for &(entity, id) in todo.iter() {
        let known = world
            .get_resource::<SmartObjectDefs>()
            .is_some_and(|defs| defs.slot(id).is_some());
        if !known {
            load_definition(world, id);
        }
        let definition = world
            .get_resource::<SmartObjectDefs>()
            .and_then(|defs| defs.slot(id))
            .and_then(|slot| slot.definition.clone());
        if let Some(definition) = definition {
            world.entity_mut(entity).insert(SmartObjectDefinition(definition));
        }
    }
}
