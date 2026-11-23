// SPDX-License-Identifier: MPL-2.0
//! "Keep simulation changes" (`docs/specs/19-editor.md` § 4, ADR-7205): copies the selected
//! entities' play-world state back to the edit world as **one** undo step.
//!
//! The play world is a copy of the edit world spawned by guid ([`crate::pie::start_play`]), so a
//! play entity and its edit counterpart share an `EntityGuid`. For each selected play entity, every
//! reflected component the **edit** entity already has is compared with the play value; each one
//! that differs becomes a whole-component [`SetProperty`], and all of them go on the undo stack as
//! one [`CommandGroup`]. Components only the play world has (physics handles, velocities, a game's
//! runtime state) are not added to the edit entity, and the derived ones (`GlobalTransform`,
//! `PreviousGlobalTransform`,
//! hierarchy links, identity) are never copied: they follow from what is copied.
//!
//! Agents call `editor.keep_simulation_changes` (optional `guids`; default: the play world's
//! selection). The book page is `book/agents/pie-modes.md`.

use std::collections::HashMap;

use regia_core::id::Guid;
use regia_scene::components::EntityGuid;

use crate::command::{CommandGroup, EditorCommand, SetProperty, UndoStack};
use crate::engine::ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, ReflectComponent};

/// Component types never copied back: derived from what is copied, or identity.
const SKIPPED_TYPE_SUFFIXES: [&str; 7] = [
    "::GlobalTransform",
    "::PreviousGlobalTransform",
    "::EntityGuid",
    "::ChildOf",
    "::Children",
    "::LevelMember",
    "::Name",
];

/// What [`keep_simulation_changes`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeepReport {
    /// Edit entities that received at least one component (by guid).
    pub entities: Vec<Guid>,
    /// Components copied, in total.
    pub components: usize,
    /// The type path of each copied component, in copy order.
    pub copied: Vec<String>,
    /// Requested guids with no play-world or no edit-world entity.
    pub missing: Vec<Guid>,
    /// Whether an undo step was pushed (false when nothing differed).
    pub undo_step: bool,
}

/// Errors of [`keep_simulation_changes`] (agent error codes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepError {
    /// `keep_nothing_selected`.
    pub code: &'static str,
    /// Human text.
    pub message: String,
}

/// The guids of the play world's selection, in selection order.
#[must_use]
pub fn play_selection_guids(play: &World) -> Vec<Guid> {
    let Some(selection) = play.get_resource::<crate::selection::Selection>() else {
        return Vec::new();
    };
    selection
        .entities()
        .iter()
        .filter_map(|entity| play.get::<EntityGuid>(*entity).map(|guid| guid.0))
        .collect()
}

fn by_guid(world: &mut World) -> HashMap<Guid, Entity> {
    world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .map(|(entity, guid)| (guid.0, entity))
        .collect()
}

/// Copies the play-world state of `guids` (default: the play world's selection) onto the edit
/// world's entities with the same guid, as one undo step on the edit world's [`UndoStack`].
///
/// # Errors
///
/// `keep_nothing_selected` when no guid was given and nothing is selected in the play world.
pub fn keep_simulation_changes(
    edit: &mut World,
    play: &mut World,
    guids: Option<&[Guid]>,
) -> Result<KeepReport, KeepError> {
    let guids: Vec<Guid> = match guids {
        Some(list) => list.to_vec(),
        None => play_selection_guids(play),
    };
    if guids.is_empty() {
        return Err(KeepError {
            code: "keep_nothing_selected",
            message: "select entities in the play world (or pass `guids`) to keep their simulation state"
                .to_string(),
        });
    }
    let play_ids = by_guid(play);
    let edit_ids = by_guid(edit);
    let registry = edit.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();

    let mut report = KeepReport::default();
    let mut group = CommandGroup::new("Keep simulation changes");
    for guid in guids {
        let (Some(&play_entity), Some(&edit_entity)) = (play_ids.get(&guid), edit_ids.get(&guid)) else {
            report.missing.push(guid);
            continue;
        };
        let Ok(edit_ref) = edit.get_entity(edit_entity) else {
            report.missing.push(guid);
            continue;
        };
        let Ok(play_ref) = play.get_entity(play_entity) else {
            report.missing.push(guid);
            continue;
        };
        let mut copied = Vec::new();
        for (_, info) in edit.inspect_entity(edit_entity).into_iter().flatten() {
            let Some(type_id) = info.type_id() else {
                continue;
            };
            let Some(registration) = registry.get(type_id) else {
                continue;
            };
            let path = registration.type_info().type_path();
            if SKIPPED_TYPE_SUFFIXES.iter().any(|suffix| path.ends_with(suffix)) {
                continue;
            }
            let Some(data) = registration.data::<ReflectComponent>() else {
                continue;
            };
            let (Some(old), Some(new)) = (data.reflect(edit_ref), data.reflect(play_ref)) else {
                continue;
            };
            if old.reflect_partial_eq(new.as_partial_reflect()) == Some(true) {
                continue;
            }
            let Ok(value) = crate::command::clone_partial(new.as_partial_reflect()) else {
                continue;
            };
            copied.push((path.to_string(), value));
        }
        if copied.is_empty() {
            continue;
        }
        report.entities.push(guid);
        for (component, value) in copied {
            match SetProperty::capture(edit, edit_entity, &component, "", value) {
                Ok(mut command) => {
                    command.coalesce = false;
                    group.push(Box::new(command));
                    report.components += 1;
                    report.copied.push(component);
                }
                Err(e) => tracing::warn!(target: "regia_editor", "keep simulation changes: {component}: {e}"),
            }
        }
    }
    drop(registry);
    if !group.commands.is_empty() {
        group.apply(edit);
        let mut stack = edit.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
        stack.seal();
        stack.push(Box::new(group));
        stack.seal();
        report.undo_step = true;
    }
    tracing::info!(
        target: "regia_editor",
        entities = report.entities.len(),
        components = report.components,
        "kept simulation changes"
    );
    Ok(report)
}
