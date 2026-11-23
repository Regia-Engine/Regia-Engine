// SPDX-License-Identifier: MPL-2.0
//! The Inspector's prefab-variant overrides (`docs/specs/83-prefab-profiler-volumes.md` § 2.4):
//! for an entity of a live instance of a variant, each component lists the fields the variant file
//! overrides, with a marker and *Clear override*, which runs `editor.set_variant_override` with
//! `value: null` (the same file, the same undo step; `book/agents/prefab-variants.md`).
//!
//! The overrides come from the registered variant in the [`PrefabLibrary`] (no disk read per
//! frame); the file is looked up by id only when *Clear override* is clicked.

use regia_core::id::Guid;
use regia_ecs::prelude::*;
use regia_scene::prefab::{PrefabInstance, PrefabLink};
use regia_scene::prelude::PrefabLibrary;

/// One override a variant file holds for one entity and component.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VariantOverrideRow {
    /// The variant's asset id.
    pub variant: Guid,
    /// The overridden entity's local id.
    pub target: Guid,
    /// The component type path.
    pub component: String,
    /// The field path (`"."` for the whole component).
    pub path: String,
    /// The value as RON.
    pub value: String,
}

/// The instance root `entity` belongs to (itself or the nearest ancestor with a
/// [`PrefabInstance`]), within a bounded walk.
fn instance_root(world: &World, entity: Entity) -> Option<Entity> {
    let mut cursor = entity;
    for _ in 0..256 {
        if world.get::<PrefabInstance>(cursor).is_some() {
            return Some(cursor);
        }
        cursor = world.get::<ChildOf>(cursor)?.parent();
    }
    None
}

/// The overrides of `component` on `entity` that the variant its instance instantiates holds
/// (empty when the instance is not of a variant, or the variant overrides nothing there). The
/// instance root's own overrides are those whose target is the root's local id.
#[must_use]
pub fn variant_overrides(world: &World, entity: Entity, component: &str) -> Vec<VariantOverrideRow> {
    let Some(root) = instance_root(world, entity) else {
        return Vec::new();
    };
    let Some(asset) = world.get::<PrefabInstance>(root).map(PrefabInstance::prefab_guid) else {
        return Vec::new();
    };
    let Some(variant) = world
        .get_resource::<PrefabLibrary>()
        .and_then(|library| library.variant(asset).cloned())
    else {
        return Vec::new();
    };
    let target = world.get::<PrefabLink>(entity).map(PrefabLink::guid);
    variant
        .edits
        .overrides
        .iter()
        .filter(|entry| entry.component == component && target == Some(entry.target))
        .map(|entry| VariantOverrideRow {
            variant: asset,
            target: entry.target,
            component: entry.component.clone(),
            path: entry.path.clone(),
            value: entry.value.get_ron().to_owned(),
        })
        .collect()
}

/// Clears one variant override: finds the variant file by id under the project and runs
/// `editor.set_variant_override`'s code with no value. Returns how many instances updated.
///
/// # Errors
/// `no_project`, `variant_missing` (no file under `assets/` has the id), and the errors of
/// [`crate::prefab_variant_edit::set_variant_override`].
pub fn clear_variant_override(
    world: &mut World,
    row: &VariantOverrideRow,
) -> Result<usize, crate::prefab_variant_edit::VariantEditError> {
    let root = crate::mesh_source::project_root(world).ok_or_else(|| {
        crate::prefab_variant_edit::VariantEditError {
            reason: "no_project",
            message: "no project is open".into(),
        }
    })?;
    let path = crate::prefab_variant_edit::find_prefab_by_id(&root, row.variant).ok_or_else(|| {
        crate::prefab_variant_edit::VariantEditError {
            reason: "variant_missing",
            message: format!("no variant file under assets/ has the id {}", row.variant),
        }
    })?;
    crate::prefab_variant_edit::set_variant_override(
        world,
        &path,
        row.target,
        &row.component,
        &row.path,
        None,
    )
}

/// The Inspector hook: under `component` of `entity`, the variant's overrides with a marker and
/// *Clear override*. Draws nothing for an entity that is not part of a variant instance.
pub fn variant_override_controls(ui: &mut egui::Ui, world: &mut World, entity: Entity, component: &str) {
    let rows = variant_overrides(world, entity, component);
    if rows.is_empty() {
        return;
    }
    let mut clear = None;
    ui.separator();
    ui.weak("Overridden by the prefab variant:");
    for row in &rows {
        ui.horizontal(|ui| {
            ui.strong("\u{25cf}")
                .on_hover_text("This field differs from the variant's base");
            let field = if row.path == "." || row.path.is_empty() {
                "(whole component)".to_owned()
            } else {
                row.path.clone()
            };
            ui.label(format!("{field} = {}", row.value));
            if crate::agent_parity::action_button(ui, &crate::agent_parity::INSPECTOR_CLEAR_VARIANT_OVERRIDE)
                .clicked()
            {
                clear = Some(row.clone());
            }
        });
    }
    if let Some(row) = clear
        && let Err(err) = clear_variant_override(world, &row)
    {
        tracing::error!(target: "regia_editor", "clear variant override: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No instance, no rows: the hook is a no-op on plain entities.
    #[test]
    fn a_plain_entity_has_no_variant_overrides() {
        let mut world = World::new();
        let plain = world.spawn_empty().id();
        assert!(variant_overrides(&world, plain, "regia_scene::transform::Transform").is_empty());
    }
}
