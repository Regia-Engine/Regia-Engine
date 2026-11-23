// SPDX-License-Identifier: MPL-2.0
//! The Terrain panel's edit layers (P-236, ADR-7825, `book/agents/terrain-editing.md`).
//!
//! The stack itself is [`regia_terrain::TerrainEditLayers`] on the terrain entity. This module is
//! the editor's side of it: the Layers list in the Terrain panel ([`layers_ui`]), the undo entries
//! for stack changes and layered strokes ([`drain_layer_log`]), routing the tool's height strokes
//! into the active layer ([`stroke_command`]) and the `<map>.terrain.editlayers` sidecar beside the
//! level ([`load_level_edit_layers`], [`save_edit_layers`]).
//!
//! Every stack change, from the panel or from an agent (`terrain.edit_layer_*`), goes through
//! [`regia_terrain::apply_op`], which logs it on [`TerrainEditLayerLog`]; the frame drains the
//! log onto the [`UndoStack`], so the panel and agents share one path and one undo history.

use std::path::{Path, PathBuf};

use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::*;
use regia_terrain::edit_layers::{
    absorb_world_stroke, apply_op, edit_layers_path, install_stack, load_edit_layers,
};
use regia_terrain::{
    EditLayerBlend, EditLayerError, EditLayerLogEntry, EditLayerOp, EditLayerPatch, ProcgenLayerTarget,
    StrokeUndo, Terrain, TerrainEditLayerLog, TerrainEditLayers,
};

use crate::command::{EditorCommand, UndoStack};
use crate::terrain_tool::TerrainStrokeCommand;

/// A stack change (create, set, delete, reorder, flatten) on the undo stack: the stack before and
/// after. Undo and redo recompute only the samples of the layers that differ.
pub struct TerrainLayerStackCommand {
    entity: Entity,
    before: Option<Box<TerrainEditLayers>>,
    after: Box<TerrainEditLayers>,
    label: String,
}

impl TerrainLayerStackCommand {
    fn install(&self, world: &mut World, next: Option<TerrainEditLayers>) {
        let Some(target) = crate::command::resolve_entity(world, self.entity.to_bits()) else {
            tracing::warn!(target: "regia_editor", "terrain layer undo: entity {} is gone", self.entity.to_bits());
            return;
        };
        let live = world.get::<TerrainEditLayers>(target).cloned();
        if let Err(err) = install_stack(world, target, live.as_ref(), next) {
            tracing::warn!(target: "regia_editor", "terrain layer undo: {err}");
        }
    }
}

impl EditorCommand for TerrainLayerStackCommand {
    fn apply(&mut self, world: &mut World) {
        self.install(world, Some((*self.after).clone()));
    }

    fn revert(&mut self, world: &mut World) {
        self.install(world, self.before.as_deref().cloned());
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        self.label.clone()
    }
}

/// Moves the edit-layer log (panel and agent changes) onto the undo stack.
pub fn drain_layer_log(world: &mut World) {
    let Some(mut log) = world.get_resource_mut::<TerrainEditLayerLog>() else {
        world.insert_resource(TerrainEditLayerLog {
            drained: true,
            ..TerrainEditLayerLog::default()
        });
        return;
    };
    log.drained = true;
    if log.entries.is_empty() {
        return;
    }
    let entries = std::mem::take(&mut log.entries);
    let mut stack = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
    for entry in entries {
        match entry {
            EditLayerLogEntry::Stroke {
                entity,
                stroke,
                layer,
            } => {
                stack.push(Box::new(TerrainStrokeCommand::with_layer(entity, stroke, layer)));
            }
            EditLayerLogEntry::Stack {
                entity,
                before,
                after,
                label,
            } => stack.push(Box::new(TerrainLayerStackCommand {
                entity,
                before,
                after,
                label,
            })),
        }
    }
}

/// The undo entry of a finished tool stroke on `entity`: a height stroke on a terrain with edit
/// layers is moved into the active layer first. `None` when the stroke changed nothing or the
/// active layer refused it (the stroke is reverted then).
pub fn stroke_command(world: &mut World, entity: Entity, undo: StrokeUndo) -> Option<Box<dyn EditorCommand>> {
    match absorb_world_stroke(world, entity, None, undo) {
        Ok((undo, layer)) if !undo.is_empty() => Some(Box::new(match layer {
            Some(layer) => TerrainStrokeCommand::with_layer(entity, undo, layer),
            None => TerrainStrokeCommand::new(entity, undo),
        })),
        Ok(_) => None,
        Err(err) => {
            tracing::warn!(target: "regia_editor", "terrain stroke refused by its edit layer: {err}");
            None
        }
    }
}

/// Whether a height stroke on `entity` may start: its active edit layer must be unlocked and
/// visible (a terrain without layers always may).
///
/// # Errors
///
/// The active layer's refusal (`edit_layer_locked`, `edit_layer_hidden`).
pub fn check_stroke_target(world: &World, entity: Entity) -> Result<(), EditLayerError> {
    match world.get::<TerrainEditLayers>(entity) {
        Some(stack) => stack.check_target(stack.active()).map(|_| ()),
        None => Ok(()),
    }
}

/// Which level's sidecar was brought in, and for which terrain.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct EditLayersLoaded {
    /// The level and terrain.
    pub loaded: Option<(PathBuf, Entity)>,
    /// The file exists and was refused (stale, newer or malformed): it is never overwritten.
    pub refused: Option<String>,
}

/// Brings the open level's `<map>.terrain.editlayers` in, once per level and terrain.
pub fn load_level_edit_layers(world: &mut World) {
    let Some(level) = world
        .get_resource::<crate::level_io::OpenLevel>()
        .and_then(|open| open.path.clone())
    else {
        return;
    };
    let mut query = world.query_filtered::<Entity, With<Terrain>>();
    let Some(entity) = query.iter(world).min_by_key(|e| e.to_bits()) else {
        return;
    };
    let key = Some((level.clone(), entity));
    if world
        .get_resource::<EditLayersLoaded>()
        .is_some_and(|l| l.loaded == key)
    {
        return;
    }
    let file = edit_layers_path(&crate::terrain_panel::side_file_base(world, &level));
    let mut refused = None;
    if let Some(terrain) = world.get::<Terrain>(entity) {
        match load_edit_layers(&file, &terrain.map) {
            Ok(Some(stack)) => {
                world.entity_mut(entity).insert(stack);
            }
            Ok(None) => {
                // A level without a file starts with no layers.
                world.entity_mut(entity).remove::<TerrainEditLayers>();
            }
            Err(err) => {
                tracing::warn!(target: "regia_editor", "{}: {err}; the layers file is left as it is", file.display());
                refused = Some(format!("{}: {err}", err.code()));
                world.entity_mut(entity).remove::<TerrainEditLayers>();
            }
        }
    }
    world.insert_resource(EditLayersLoaded { loaded: key, refused });
}

/// Writes `<map>.terrain.editlayers` beside `level` when the stack changed since the last save.
/// A terrain whose layers were all removed writes an empty stack, so the old file never comes
/// back. `commit` clears the dirty flag. Returns the file written.
///
/// # Errors
///
/// The stack cannot be encoded or the file cannot be written, or two terrains have unsaved layers.
pub fn save_edit_layers(world: &mut World, level: &Path, commit: bool) -> Result<Option<PathBuf>, String> {
    if let Some(reason) = world
        .get_resource::<EditLayersLoaded>()
        .and_then(|l| l.refused.clone())
    {
        tracing::warn!(target: "regia_editor", "the layers file was refused ({reason}); it is left as it is");
        return Ok(None);
    }
    reconcile_before_save(world)?;
    let mut query = world.query::<(Entity, &TerrainEditLayers)>();
    let unsaved: Vec<(Entity, Vec<u8>)> = query
        .iter(world)
        .filter(|(_, s)| s.unsaved())
        .map(|(e, s)| s.to_bytes().map(|b| (e, b)))
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    if unsaved.len() > 1 {
        return Err(format!(
            "{} terrains have unsaved edit layers and a level holds one layers file",
            unsaved.len()
        ));
    }
    let Some((entity, bytes)) = unsaved.into_iter().next() else {
        return Ok(None);
    };
    let file = edit_layers_path(&crate::terrain_panel::side_file_base(world, level));
    crate::level_io::write_atomic(&file, &bytes)?;
    if commit && let Some(mut stack) = world.get_mut::<TerrainEditLayers>(entity) {
        stack.mark_saved();
    }
    world.insert_resource(EditLayersLoaded {
        loaded: Some((level.to_path_buf(), entity)),
        refused: None,
    });
    Ok(Some(file))
}

/// Reconciles raw writes before either the cooked terrain or its sidecar is saved.
pub(crate) fn reconcile_before_save(world: &mut World) -> Result<(), String> {
    let entities: Vec<Entity> = world
        .query_filtered::<Entity, (With<Terrain>, With<TerrainEditLayers>)>()
        .iter(world)
        .collect();
    for entity in entities {
        regia_terrain::edit_layers::reconcile_world_base(world, entity).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Clears the unsaved flag of every stack (after an orchestrated save stored the level path).
pub fn commit_edit_layers_save(world: &mut World) {
    let mut query = world.query::<&mut TerrainEditLayers>();
    for mut stack in query.iter_mut(world) {
        if stack.unsaved() {
            stack.mark_saved();
        }
    }
}

/// The Layers section's own state (a world resource).
#[derive(Resource, Debug, Clone, Default)]
pub struct TerrainLayersPanel {
    /// Name for the next new layer.
    pub new_name: String,
    /// The next new layer is a Set height layer.
    pub new_set: bool,
    /// The rename field (for the active layer).
    pub rename: String,
    /// The layer the rename field was filled from.
    pub rename_of: Option<u32>,
    /// Opacity being dragged: `(layer, value)`, applied when the drag ends.
    pub opacity_edit: Option<(u32, f32)>,
    /// Procgen graph path for the next Attach on the active layer (ADR-8381).
    pub graph: String,
    /// Procgen output name for the next Attach on the active layer (ADR-8381).
    pub output: String,
    /// The last refusal or result.
    pub message: String,
    /// The frame (`cumulative_frame_nr`) of the last action: egui may run the panel twice in one
    /// frame, and the second pass must not repeat it.
    pub acted_frame: Option<u64>,
    /// The frame being drawn.
    frame: u64,
}

impl TerrainLayersPanel {
    /// `true` once per frame: the caller performs its action.
    fn act(&mut self) -> bool {
        if self.acted_frame == Some(self.frame) {
            return false;
        }
        self.acted_frame = Some(self.frame);
        true
    }
}

fn run(world: &mut World, entity: Entity, op: EditLayerOp, panel: &mut TerrainLayersPanel) {
    if !panel.act() {
        return;
    }
    match apply_op(world, entity, &op) {
        Ok(_) => panel.message.clear(),
        Err(err) => panel.message = format!("{}: {err}", err.code()),
    }
}

fn patch(id: u32, patch: EditLayerPatch) -> EditLayerOp {
    EditLayerOp::Set { id, patch }
}

/// The Layers section of the Terrain panel for `entity`: add, select, rename, delete, show/hide,
/// lock, reorder, opacity, material weights, procgen attach and detach, and Flatten into base.
/// Every control is a focusable widget.
pub fn layers_ui(ui: &mut egui::Ui, world: &mut World, entity: Option<Entity>) {
    use crate::agent_parity as ap;
    let mut panel = world
        .get_resource::<TerrainLayersPanel>()
        .cloned()
        .unwrap_or_default();
    panel.frame = ui.ctx().cumulative_frame_nr();
    ui.heading("Edit layers");
    let Some(entity) = entity else {
        ui.label("No terrain in the level.");
        return;
    };
    let stack = world.get::<TerrainEditLayers>(entity).cloned();
    if let Some(reason) = world
        .get_resource::<EditLayersLoaded>()
        .and_then(|l| l.refused.clone())
    {
        ui.colored_label(
            ui.visuals().error_fg_color,
            format!("Layers file not loaded: {reason}"),
        );
    }
    ui.horizontal(|ui| {
        let label = ui.label("New layer");
        ui.add(egui::TextEdit::singleline(&mut panel.new_name).desired_width(120.0))
            .labelled_by(label.id)
            .on_hover_text("Name of the next layer");
        ap::action_toggle(ui, &ap::TERRAIN_LAYER_SET_HEIGHT, &mut panel.new_set)
            .on_hover_text("A Set height layer replaces the height below it; layer order matters for it");
        if ap::action_button(ui, &ap::TERRAIN_LAYER_ADD).clicked() {
            let count = stack.as_ref().map_or(0, |s| s.layers().len());
            let name = if panel.new_name.trim().is_empty() {
                format!("Layer {}", count + 1)
            } else {
                panel.new_name.trim().to_owned()
            };
            let blend = if panel.new_set {
                EditLayerBlend::Set
            } else {
                EditLayerBlend::Add
            };
            run(world, entity, EditLayerOp::Create { name, blend }, &mut panel);
            panel.new_name.clear();
        }
    });
    let Some(stack) = stack else {
        ui.label("No edit layers: strokes write the terrain directly.");
        show_message(ui, &panel);
        world.insert_resource(panel);
        return;
    };
    let active = stack.active();
    let base_text = if active.is_none() { "Base (active)" } else { "Base" };
    if ap::action_button_text(ui, &ap::TERRAIN_LAYER_BASE, base_text)
        .on_hover_text("Strokes write the base heights")
        .clicked()
        && panel.act()
        && let Err(err) = regia_terrain::set_active_layer(world, entity, None)
    {
        panel.message = format!("{}: {err}", err.code());
    }
    let count = stack.layers().len();
    // Top of the stack first, as it is drawn over the rest.
    for (index, layer) in stack.layers().iter().enumerate().rev() {
        let id = layer.id;
        ui.horizontal(|ui| {
            let mut text = format!("{} ({})", layer.name, layer.blend.name());
            if active == Some(id) {
                text.push_str(" (active)");
            }
            if ap::action_button_text(ui, &ap::TERRAIN_LAYER_SELECT, text)
                .on_hover_text("Strokes write this layer")
                .clicked()
                && panel.act()
                && let Err(err) = regia_terrain::set_active_layer(world, entity, Some(id))
            {
                panel.message = format!("{}: {err}", err.code());
            }
            let shown = if layer.enabled { "Hide" } else { "Show" };
            if ap::action_button_text(ui, &ap::TERRAIN_LAYER_VISIBLE, shown).clicked() {
                let p = EditLayerPatch {
                    enabled: Some(!layer.enabled),
                    ..EditLayerPatch::default()
                };
                run(world, entity, patch(id, p), &mut panel);
            }
            let lock = if layer.locked { "Unlock" } else { "Lock" };
            if ap::action_button_text(ui, &ap::TERRAIN_LAYER_LOCK, lock).clicked() {
                let p = EditLayerPatch {
                    locked: Some(!layer.locked),
                    ..EditLayerPatch::default()
                };
                run(world, entity, patch(id, p), &mut panel);
            }
            if ap::action_enabled(ui, index + 1 < count, &ap::TERRAIN_LAYER_UP, None).clicked() {
                run(
                    world,
                    entity,
                    EditLayerOp::Reorder { id, index: index + 1 },
                    &mut panel,
                );
            }
            if ap::action_enabled(ui, index > 0, &ap::TERRAIN_LAYER_DOWN, None).clicked() {
                let to = index.saturating_sub(1);
                run(world, entity, EditLayerOp::Reorder { id, index: to }, &mut panel);
            }
            if ap::action_enabled(ui, !layer.locked, &ap::TERRAIN_LAYER_DELETE, None).clicked() {
                run(world, entity, EditLayerOp::Delete { id }, &mut panel);
            }
            let mut value = match panel.opacity_edit {
                Some((edit, v)) if edit == id => v,
                _ => layer.opacity,
            };
            let slider = ui
                .add(egui::Slider::new(&mut value, 0.0..=1.0).text("Opacity"))
                .on_hover_text("How strongly the layer applies");
            if slider.changed() {
                panel.opacity_edit = Some((id, value));
            }
            // Keyboard steps apply at once; a pointer drag applies once, when it ends.
            let commit = slider.drag_stopped() || (slider.changed() && !slider.dragged());
            if commit && let Some((edit, v)) = panel.opacity_edit.take() {
                let p = EditLayerPatch {
                    opacity: Some(v),
                    ..EditLayerPatch::default()
                };
                run(world, entity, patch(edit, p), &mut panel);
            }
        });
        if let Some(target) = layer.procgen() {
            ui.horizontal(|ui| {
                ui.label(format!("owned by {} · {}", target.graph, target.output))
                    .on_hover_text("Brush edits are refused until the target is detached");
                if ap::action_button(ui, &ap::TERRAIN_LAYER_DETACH).clicked() {
                    run(
                        world,
                        entity,
                        EditLayerOp::SetProcgen { id, target: None },
                        &mut panel,
                    );
                }
            });
        } else if active == Some(id) {
            ui.horizontal(|ui| {
                let graph_label = ui.label("Procgen graph");
                ui.add(egui::TextEdit::singleline(&mut panel.graph).desired_width(160.0))
                    .labelled_by(graph_label.id)
                    .on_hover_text("Project path of the procgen graph that owns this layer");
                let output_label = ui.label("Output");
                ui.add(egui::TextEdit::singleline(&mut panel.output).desired_width(80.0))
                    .labelled_by(output_label.id)
                    .on_hover_text("Named output the graph writes into this layer");
                if ap::action_button(ui, &ap::TERRAIN_LAYER_ATTACH).clicked() {
                    let target = ProcgenLayerTarget {
                        graph: panel.graph.trim().to_owned(),
                        output: panel.output.trim().to_owned(),
                    };
                    run(
                        world,
                        entity,
                        EditLayerOp::SetProcgen {
                            id,
                            target: Some(target),
                        },
                        &mut panel,
                    );
                }
            });
        }
    }
    if let Some(id) = active
        && let Some(layer) = stack.layer(id)
    {
        if panel.rename_of != Some(id) {
            panel.rename.clone_from(&layer.name);
            panel.rename_of = Some(id);
        }
        ui.horizontal(|ui| {
            let label = ui.label("Name");
            ui.add(egui::TextEdit::singleline(&mut panel.rename).desired_width(120.0))
                .labelled_by(label.id)
                .on_hover_text("New name of the active layer");
            if ap::action_button(ui, &ap::TERRAIN_LAYER_RENAME).clicked() {
                let p = EditLayerPatch {
                    name: Some(panel.rename.trim().to_owned()),
                    ..EditLayerPatch::default()
                };
                run(world, entity, patch(id, p), &mut panel);
            }
        });
        let folded = stack.folded_weights(0);
        ui.label(format!(
            "Weights at (0, 0): {:.3} {:.3} {:.3} (sum {:.3})",
            folded[0],
            folded[1],
            folded[2],
            folded.iter().sum::<f32>()
        ));
        let owned = layer.procgen().is_some();
        if ap::action_enabled(ui, !owned, &ap::TERRAIN_LAYER_PAINT_WEIGHT, None)
            .on_hover_text("Paint channel 1 at sample (0, 0). Detach a procgen target first.")
            .clicked()
        {
            let mut channels = [0.0; regia_terrain::layers::MAX_LAYERS];
            channels[1] = 1.0;
            run(
                world,
                entity,
                EditLayerOp::PaintWeights {
                    layer: Some(id),
                    index: 0,
                    channels,
                },
                &mut panel,
            );
        }
    }
    if ap::action_enabled(ui, !stack.layers().is_empty(), &ap::TERRAIN_LAYER_FLATTEN, None)
        .on_hover_text("Bake every shown layer into the base heights")
        .clicked()
    {
        run(world, entity, EditLayerOp::Flatten, &mut panel);
    }
    ui.label(format!(
        "{} samples recomputed since the layers were made",
        world
            .get::<TerrainEditLayers>(entity)
            .map_or(0, TerrainEditLayers::recomputed_samples)
    ));
    show_message(ui, &panel);
    world.insert_resource(panel);
}

fn show_message(ui: &mut egui::Ui, panel: &TerrainLayersPanel) {
    if !panel.message.is_empty() {
        ui.colored_label(ui.visuals().error_fg_color, &panel.message);
    }
}

#[cfg(test)]
mod tests;
