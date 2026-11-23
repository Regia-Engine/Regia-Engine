// SPDX-License-Identifier: MPL-2.0
//! The asset editors registered for document autosave at startup (`crate::doc_autosave`,
//! ADR-9770). Each provider lives next to its editor, as a child module with access to the
//! editor's saved copy.

use crate::doc_autosave::DocAutosaveRegistry;

/// Registers every built-in asset editor that has a per-document dirty flag.
pub fn register_builtin(registry: &mut DocAutosaveRegistry) {
    registry.register(crate::matgraph_editor::autosave::PROVIDER);
    registry.register(crate::ui_designer::autosave::PROVIDER);
    registry.register(crate::procgen::autosave::PROVIDER);
    registry.register(crate::bt_editor::autosave::PROVIDER);
    registry.register(crate::state_tree_editor::autosave::PROVIDER);
    registry.register(crate::tables_panel::autosave::PROVIDER);
    registry.register(crate::cue_editor_panel::autosave::PROVIDER);
    registry.register(crate::vfx_editor::autosave::PROVIDER);
    registry.register(crate::conversation_panel::autosave::PROVIDER);
    registry.register(crate::anim::autosave::PROVIDER);
    registry.register(crate::ui_anim_autosave::PROVIDER);
    registry.register(crate::seq_autosave::PROVIDER);
    registry.register(crate::prefab_autosave::PROVIDER);
    registry.register_prepare(crate::prefab_autosave::KIND, crate::prefab_autosave::prepare);
    registry.register(crate::shot_document::PROVIDER);
    // Sprint w11-forms-undo-autosave: cloth, ragdoll, vehicle, abilities, loc.
    registry.register(forms::CLOTH);
    registry.register(forms::RAGDOLL);
    registry.register(forms::VEHICLE);
    registry.register(forms::ABILITIES);
    registry.register(forms::LOC);
    registry.register(crate::camera_panel::history::PROVIDER);
    registry.register(crate::smart_object_def_panel::autosave::PROVIDER);
    registry.register(crate::inspector::inline_asset::PROVIDER);
    registry.register(crate::mm_db_editor::autosave::PROVIDER);
    // P-322 part b: the tables and vehicles kept open behind the shown one.
    registry.register_retained(crate::tables_panel::autosave::RETAINED);
    registry.register_retained(forms::VEHICLE_RETAINED);
    register_saves(registry);
}

/// The save hooks of the built-in editors (P-147, ADR-8147): what the Save all dialog and
/// `editor.save_all` call for each listed kind, beside its editor's own Save.
fn register_saves(registry: &mut DocAutosaveRegistry) {
    registry.register_save(
        crate::matgraph_editor::autosave::KIND,
        crate::matgraph_editor::autosave::save,
        false,
    );
    registry.register_save(
        crate::ui_designer::autosave::KIND,
        crate::ui_designer::autosave::save,
        false,
    );
    registry.register_save(
        crate::procgen::autosave::KIND,
        crate::procgen::autosave::save,
        false,
    );
    registry.register_save(
        crate::bt_editor::autosave::KIND,
        crate::bt_editor::autosave::save,
        false,
    );
    registry.register_save(
        crate::state_tree_editor::autosave::KIND,
        crate::state_tree_editor::autosave::save,
        false,
    );
    registry.register_save(
        crate::tables_panel::autosave::KIND,
        crate::tables_panel::autosave::save,
        false,
    );
    registry.register_save(
        crate::cue_editor_panel::autosave::KIND,
        crate::cue_editor_panel::autosave::save,
        false,
    );
    registry.register_save(
        crate::vfx_editor::autosave::KIND,
        crate::vfx_editor::autosave::save,
        false,
    );
    registry.register_save(
        crate::conversation_panel::autosave::KIND,
        crate::conversation_panel::autosave::save,
        false,
    );
    registry.register_save(crate::anim::autosave::KIND, crate::anim::autosave::save, false);
    registry.register_save(
        crate::ui_anim_autosave::KIND,
        crate::ui_anim_autosave::save,
        false,
    );
    registry.register_save(crate::seq_autosave::KIND, crate::seq_autosave::save, false);
    registry.register_save(crate::prefab_autosave::KIND, crate::prefab_autosave::save, false);
    registry.register_save(crate::shot_document::KIND, crate::shot_document::save_hook, false);
    registry.register_save(forms::CLOTH_KIND, forms::cloth_save, false);
    registry.register_save(forms::RAGDOLL_KIND, forms::ragdoll_save, false);
    registry.register_save(forms::VEHICLE_KIND, forms::vehicle_save, false);
    registry.register_save(forms::ABILITIES_KIND, forms::abilities_save, false);
    // The edit box has no file: applying it writes the project's string tables.
    registry.register_save(forms::LOC_KIND, forms::loc_save, true);
    registry.register_save(
        crate::camera_panel::history::KIND,
        crate::camera_panel::history::save,
        false,
    );
    registry.register_save(
        crate::smart_object_def_panel::autosave::KIND,
        crate::smart_object_def_panel::autosave::save,
        false,
    );
    // Motion databases joined the provider list on integ-v06m (ADR-9770). Save all uses the
    // same editor save the panel does (content-save-bulk, ADR-8147).
    registry.register_save(
        crate::mm_db_editor::autosave::KIND,
        crate::mm_db_editor::autosave::save,
        false,
    );
    registry.register_save(
        crate::inspector::inline_asset::KIND,
        crate::inspector::inline_asset::save_hook,
        false,
    );
}

mod forms;
