// SPDX-License-Identifier: MPL-2.0
//! The UI designer panel (ADR-1811, ADR-2489, `docs/specs/81-ui-designer.md` § 4.4): the palette and
//! hierarchy on the left, the laid-out preview in the middle, the inspector and lint on the right. Every change goes
//! through [`UiDesign::apply`] (or [`UiDesign::apply_merged`] for a running drag), so the panel and
//! the agent methods share one undo history.

use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use regia_ui::authoring::{
    AnimationBindDef, BackgroundDef, BindDef, BindTargetDef, LocSpan, RepeatRowHeight, ScreenBackDef,
    TextSpanDef, UiNodeDef,
};
use regia_ui::style::{Align, Edges, FlexDirection as Dir, JustifyContent as Justify, PositionType};
use regia_ui::{BindConverter, BindMode, Dimension, Display, UiColor, ViewModels};
use serde_json::{Value, json};

use crate::ui_designer::{
    DesignOp, DesignerError, DropZone, LintFinding, NodePath, PRESETS, UiDesign, UiDesigner, drop_add,
    drop_move, enclosing_repeats, item_fields_at, lint, node_at, node_kind, path_string, preset,
    repeat_op_with, sample_key, walk,
};
use crate::ui_designer_canvas::{
    AlignKind, Axis, CanvasView, Guide, Handle, SnapSettings, TransformMode, TransformRequest, device_frame,
    plan_group_move, plan_transform, rect_of, ruler_step,
};
use crate::ui_designer_component::{
    Exposed, assets_root, instance_node, load_component, make_component, palette_components,
    set_instance_param, set_instance_slot,
};
use crate::ui_designer_gpu::{ThemeChoice, preview_device, project_theme_files, theme_names, themed_preview};
use crate::ui_designer_multi::{copy_text, parse_clipboard, paste_target, shared_fields};
use crate::ui_designer_preview::{
    MAX_REPEAT_COUNT, PreviewFrame, PreviewSettings, SIZE_PRESETS, preview, project_localization,
};
use crate::ui_designer_widget::{FieldType, widget_field, widget_fields};

/// What a palette chip carries while it is dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaletteNode(&'static str);

/// What a component chip carries while it is dragged: the component's `use` path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PaletteComponent(String);

/// The read-only rows of one instance: its path, the inherited nodes `(depth, label)` and the
/// expansion's diagnostics as text.
type InheritedRows = (NodePath, Vec<(usize, String)>, Vec<String>);

/// The hierarchy rows and the (document serial, revision) they were built for.
type TreeCache = ((u64, u64), std::sync::Arc<[TreeRow]>);

/// One selected node's attribution, cached by document identity and runtime frame.
type StyleSourcesCache = ((u64, u64, u64, u64, String), Value);

/// Panel-only state: text fields and the cached preview frame.
#[derive(Resource, Default)]
pub struct UiDesignerPanelState {
    /// The Animations button was pressed this frame (opens the UI Animation panel).
    open_animations: bool,
    open_path: String,
    field_key: String,
    field_value: String,
    preset: usize,
    cached: Option<(u64, PreviewSettings, PreviewFrame)>,
    scale_revision: u64,
    lint: Option<(u64, Vec<LintFinding>)>,
    /// Draw the preview with the runtime renderer and the project theme (ADR-2715).
    themed: bool,
    /// The device and themes of the themed preview, gathered from the world each frame it is on.
    themed_inputs: Option<(regia_render::RenderDevice, ThemeChoice)>,
    /// The last themed frame, uploaded as a texture, and what it was drawn for.
    themed_cache: Option<(u64, PreviewSettings, egui::TextureHandle)>,
    themed_error: Option<String>,
    /// The theme picker's names (ADR-7677) and the project theme files they were read from.
    theme_names: Option<(Vec<PathBuf>, Vec<String>)>,
    /// The component palette: the `assets/` directory it was scanned for and what it found (spec 111).
    components: Option<(Option<PathBuf>, Vec<regia_ui::ui_component::ComponentInfo>)>,
    /// Each instance's inherited rows, for one document revision.
    inherited: Option<(u64, Vec<InheritedRows>)>,
    /// The hierarchy rows, for one (document serial, revision).
    tree: Option<TreeCache>,
    /// Make component: the new component's path under `assets/`.
    make_path: String,
    /// Make component: the selected node's properties to expose as parameters.
    make_expose: Vec<regia_ui::ui_component::ParamProperty>,
    /// A component to open after this frame (enter).
    enter: Option<PathBuf>,
    /// The picture picker's list of project pictures (ADR-3148), read once until Refresh.
    pictures: Option<Vec<String>>,
    /// Icon names of the editor's icon atlas and the atlas generation they were read at.
    icons: (u64, Vec<String>),
    /// Source pictures decoded for the schematic preview, and their textures by manifest path.
    picture_cache: crate::ui_designer_images::PictureCache,
    picture_textures: std::collections::HashMap<String, Option<egui::TextureHandle>>,
    /// The preview canvas's zoom and pan (ADR-9730).
    view: CanvasView,
    /// Snapping is off (the toolbar's Snap box; Alt while dragging also turns it off).
    no_snap: bool,
    /// Grid spacing in UI px (0: no grid).
    grid: f32,
    /// Rulers along the canvas's top and left edge are hidden.
    hide_rulers: bool,
    /// The device frame around phone and handheld sizes is hidden.
    hide_device: bool,
    /// The running move or resize in the preview.
    drag: Option<CanvasDrag>,
    /// Guide lines of the running drag's snaps (editor state, never saved).
    guides: Vec<Guide>,
    /// The egui frame whose input (wheel, pan, edit keys) was used: egui may run the panel twice in
    /// one frame, and the input must act once.
    input_frame: Option<u64>,
    /// This pass is the first of its frame.
    fresh_input: bool,
    /// Anchor picker: zero offsets instead of preserving the measured rectangle.
    snap_anchor_position: bool,
    /// Input theme context for the isolated preview and style source inspector.
    runtime_choice: ThemeChoice,
    /// A token source requested by the Style section.
    theme_jump: Option<(String, String)>,
    /// Cached source attribution; no replay while the section is closed or idle.
    style_sources: Option<StyleSourcesCache>,
    /// Texture of the current interactive runtime frame.
    live_texture: Option<egui::TextureHandle>,
    /// Draw the declared focus neighbors as arrows over the preview (ADR-7827; panel state only).
    show_nav: bool,
    /// The Navigation section's armed Pick: the next preview click names that direction's neighbor
    /// of the selected node instead of selecting.
    nav_pick: Option<&'static str>,
}

/// A move or resize in the preview: what was held and the document and layout it started from.
struct CanvasDrag {
    node: NodePath,
    handle: Handle,
    /// The node's rect when the drag started (frame px).
    start_rect: [f32; 4],
    /// The pointer when the drag started (screen points).
    start_pointer: egui::Pos2,
    file: regia_ui::UiFile,
    frame: PreviewFrame,
    /// The rect shown with its numbers (frame px).
    live: [f32; 4],
    /// A move of a node in the flow is applied on release (a reorder changes paths).
    pending: Option<DesignOp>,
    /// A body drag of one of several selected nodes moves them all (ADR-9980): the selection.
    group: Option<Vec<NodePath>>,
    /// The group drag's guide lines.
    guides: Vec<Guide>,
}

/// One frame of a group drag (ADR-9980): every top-level selected node moves by the pointer's
/// travel from the document the drag started from, snapped on the group's bounding rect, merged into
/// one undo step; back at the start, the start styles. The live rect is the group's box.
fn group_drag(
    doc: &mut UiDesign,
    drag: &mut CanvasDrag,
    group: &[NodePath],
    delta: [f32; 2],
    snap: &SnapSettings,
    error: &mut Option<String>,
) {
    let mut plan = match plan_group_move(&drag.file.root, &drag.frame, group, delta, snap) {
        Ok(plan) => plan,
        Err(e) => {
            *error = Some(e.to_string());
            return;
        }
    };
    drag.live = plan.rect;
    drag.guides = std::mem::take(&mut plan.guides);
    let style_of = |root: &UiNodeDef, path: &[usize]| {
        node_at(root, path).map(|n| {
            crate::ui_designer::node_json(n)
                .get("style")
                .cloned()
                .unwrap_or(Value::Null)
        })
    };
    // Every moved node's style as it should be now: planned, or the start style when it stays.
    let ops: Vec<DesignOp> = plan
        .nodes
        .iter()
        .filter_map(|path| {
            let planned = plan.ops.iter().find_map(|op| match op {
                DesignOp::Set { node, value, .. } if node == path => Some(value.clone()),
                _ => None,
            });
            let value = planned.or_else(|| style_of(&drag.file.root, path))?;
            Some(DesignOp::Set {
                node: path.clone(),
                key: "style".to_owned(),
                value,
            })
        })
        .collect();
    let unchanged = ops.iter().all(|op| {
        matches!(op, DesignOp::Set { node, value, .. } if style_of(&doc.file().root, node).as_ref() == Some(value))
    });
    if !unchanged {
        plan.ops = ops;
        report(error, doc.move_many(plan, group, Some("canvas-group-drag")));
    }
}

/// Draws the UI designer tab.
pub fn ui_designer_panel(ui: &mut egui::Ui, world: &mut World) {
    let _span = tracing::trace_span!("ui_designer_panel").entered();
    let mut designer = world.remove_resource::<UiDesigner>().unwrap_or_default();
    let mut state = world
        .remove_resource::<UiDesignerPanelState>()
        .unwrap_or_default();
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone());
    let models = world.get_resource::<ViewModels>().cloned();
    if let Some(atlas) = world.get_resource::<regia_ui::icons::IconAtlas>()
        && atlas.generation != state.icons.0
    {
        state.icons = (
            atlas.generation,
            atlas.names().into_iter().map(str::to_owned).collect(),
        );
    }
    state.themed_inputs = None;
    let playing = world
        .get_non_send::<crate::ui_designer_interactive::DesignerRuntime>()
        .is_some_and(|runtime| runtime.interactive);
    if state.themed || playing {
        // The theme the preview draws with is the document's pick (`PreviewSettings::theme`, the
        // picker below), else the active one (ADR-7677).
        let choice = ThemeChoice {
            themes: world.get_resource::<regia_ui::UiThemes>().cloned(),
            active: world.get_resource::<regia_ui::ActiveTheme>().cloned(),
            files: root.as_deref().map(project_theme_files).unwrap_or_default(),
            theme: None,
            project: root.clone(),
        };
        if state
            .theme_names
            .as_ref()
            .is_none_or(|(files, _)| *files != choice.files)
        {
            state.theme_names = Some((choice.files.clone(), theme_names(&choice)));
        }
        match preview_device(world) {
            Ok(device) => state.themed_inputs = Some((device, choice)),
            Err(e) => state.themed_error = Some(e.to_string()),
        }
    }
    state.runtime_choice = state
        .themed_inputs
        .as_ref()
        .map(|(_, choice)| choice.clone())
        .unwrap_or_else(|| ThemeChoice {
            themes: world.get_resource::<regia_ui::UiThemes>().cloned(),
            active: world.get_resource::<regia_ui::ActiveTheme>().cloned(),
            files: if state.runtime_choice.project == root {
                state.runtime_choice.files.clone()
            } else {
                root.as_deref().map(project_theme_files).unwrap_or_default()
            },
            theme: None,
            project: root.clone(),
        });
    let mut runtime = world
        .remove_non_send::<crate::ui_designer_interactive::DesignerRuntime>()
        .unwrap_or_default();
    draw(ui, &mut designer, &mut state, &mut runtime, root, models.as_ref());
    world.insert_non_send(runtime);
    if let Some((theme, token)) = state.theme_jump.take()
        && let Err(e) =
            crate::ui_designer_interactive::open_theme_source(world, &state.runtime_choice, &theme, &token)
    {
        designer.error = Some(e.to_string());
    }
    if std::mem::take(&mut state.open_animations) {
        crate::editor_layout::LayoutRequests::push(
            world,
            crate::editor_layout::LayoutRequest::Open(crate::shell::Panel::UiAnim),
        );
    }
    world.insert_resource(designer);
    world.insert_resource(state);
}

fn draw(
    ui: &mut egui::Ui,
    designer: &mut UiDesigner,
    state: &mut UiDesignerPanelState,
    runtime: &mut crate::ui_designer_interactive::DesignerRuntime,
    project: Option<PathBuf>,
    models: Option<&ViewModels>,
) {
    ui.add_enabled_ui(!runtime.interactive, |ui| {
        toolbar(ui, designer, state, project.as_ref())
    });
    if let Some(err) = designer.error.clone() {
        ui.horizontal(|ui| {
            ui.colored_label(egui::Color32::from_rgb(230, 110, 90), err);
            if crate::a11y_audit::glyph_button(ui, "x", "Dismiss the error").clicked() {
                designer.error = None;
            }
        });
    }
    let nr = ui.ctx().cumulative_frame_nr();
    state.fresh_input = state.input_frame != Some(nr);
    state.input_frame = Some(nr);
    let keys = if state.fresh_input && !runtime.interactive {
        edit_keys(ui)
    } else {
        EditKeys::default()
    };
    let mut clipboard = designer.clipboard.take();
    let Ok(doc) = designer.doc_mut() else {
        designer.clipboard = clipboard;
        ui.label("Open a .ui.ron file or start a new document.");
        return;
    };
    if doc.comments_lost {
        ui.colored_label(
            egui::Color32::from_rgb(220, 180, 90),
            "The last save wrote the canonical form: the file's comments were not kept.",
        );
    }
    let mut error = None;
    if !runtime.interactive {
        report(&mut error, shortcuts(ui, doc));
        report(&mut error, apply_edit_keys(ui.ctx(), doc, &mut clipboard, keys));
    }
    ui.columns(3, |cols| {
        cols[0].add_enabled_ui(!runtime.interactive, |ui| {
            hierarchy(ui, doc, state, &mut clipboard, &mut error)
        });
        preview_view(&mut cols[1], doc, state, runtime, project.as_deref(), &mut error);
        inspector(
            &mut cols[2],
            doc,
            state,
            runtime,
            models,
            project.as_deref(),
            &mut error,
        );
    });
    if !ui.input(|i| i.pointer.any_down()) {
        doc.end_merge();
    }
    designer.clipboard = clipboard;
    if let Some(e) = error {
        designer.error = Some(e);
    }
    if let Some(path) = state.enter.take()
        && let Err(e) = designer.enter(&path)
    {
        designer.error = Some(e.to_string());
    }
}

/// Ctrl+Z undoes, Ctrl+Shift+Z and Ctrl+Y redo (Cmd on macOS) while the pointer is over the panel;
/// elsewhere they stay the shell's scene undo. Not while a text field has focus, which keeps its
/// own undo. Redo is tested first: `consume_key` ignores an extra Shift.
fn shortcuts(ui: &egui::Ui, doc: &mut UiDesign) -> Result<(), DesignerError> {
    if !ui.rect_contains_pointer(ui.max_rect()) || ui.ctx().text_edit_focused() {
        return Ok(());
    }
    let (redo, undo) = ui.input_mut(|i| {
        let command = egui::Modifiers::COMMAND;
        let redo = i.consume_key(command | egui::Modifiers::SHIFT, egui::Key::Z)
            || i.consume_key(command, egui::Key::Y);
        (redo, !redo && i.consume_key(command, egui::Key::Z))
    });
    if redo && doc.redo_len() > 0 {
        doc.redo()
    } else if undo && doc.undo_len() > 0 {
        doc.undo()
    } else {
        Ok(())
    }
}

/// The edit keys of one frame (ADR-9730): copy, cut, paste (with the pasted text), Ctrl+D and Delete.
#[derive(Debug, Default)]
struct EditKeys {
    copy: bool,
    cut: bool,
    paste: Option<String>,
    duplicate: bool,
    delete: bool,
}

/// Reads the edit keys while the pointer is over the panel and no text field has focus (a text
/// field keeps its own copy and paste).
fn edit_keys(ui: &egui::Ui) -> EditKeys {
    if !ui.rect_contains_pointer(ui.max_rect()) || ui.ctx().text_edit_focused() {
        return EditKeys::default();
    }
    ui.input_mut(|i| {
        let mut keys = EditKeys::default();
        for event in &i.events {
            match event {
                egui::Event::Copy => keys.copy = true,
                egui::Event::Cut => keys.cut = true,
                egui::Event::Paste(text) => keys.paste = Some(text.clone()),
                _ => {}
            }
        }
        keys.duplicate = i.consume_key(egui::Modifiers::COMMAND, egui::Key::D);
        keys.delete = i.consume_key(egui::Modifiers::NONE, egui::Key::Delete);
        keys
    })
}

/// Copy, cut and paste go through the designer's clipboard (kept across documents) and the system
/// clipboard; a pasted text that is not node JSON pastes the designer's clipboard instead.
fn apply_edit_keys(
    ctx: &egui::Context,
    doc: &mut UiDesign,
    clipboard: &mut Option<String>,
    keys: EditKeys,
) -> Result<(), DesignerError> {
    let selected: Vec<NodePath> = doc
        .selected_nodes()
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect();
    if (keys.copy || keys.cut) && !selected.is_empty() {
        let text = copy_text(&doc.file().root, &selected)?;
        ctx.copy_text(text.clone());
        *clipboard = Some(text);
        if keys.cut {
            doc.remove_many(&selected)?;
        }
    }
    if let Some(text) = keys.paste {
        let text = if parse_clipboard(&text).is_ok() {
            Some(text)
        } else {
            clipboard.clone()
        };
        if let Some(text) = text {
            let (parent, index) = paste_target(doc);
            doc.paste(&text, &parent, index)?;
        }
    }
    if keys.duplicate && !selected.is_empty() {
        doc.duplicate_many(&selected)?;
    }
    if keys.delete && !selected.is_empty() {
        doc.remove_many(&selected)?;
    }
    Ok(())
}

fn toolbar(
    ui: &mut egui::Ui,
    designer: &mut UiDesigner,
    state: &mut UiDesignerPanelState,
    project: Option<&PathBuf>,
) {
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.open_path)
                .hint_text("assets/ui/hud.ui.ron")
                .desired_width(220.0),
        );
        let path = || {
            let p = PathBuf::from(state.open_path.trim());
            match project {
                Some(root) if p.is_relative() => root.join(p),
                _ => p,
            }
        };
        if ui.button("Open").clicked()
            && let Err(e) = designer.open(&path())
        {
            designer.error = Some(e.to_string());
        }
        if ui.button("New").clicked() {
            designer.new_document();
            if !state.open_path.trim().is_empty()
                && let Ok(doc) = designer.doc_mut()
            {
                doc.path = Some(path());
            }
        }
        if designer.depth() > 0
            && ui
                .button("Return")
                .on_hover_text("Back to the document that entered this component")
                .clicked()
            && let Err(e) = designer.leave()
        {
            designer.error = Some(e.to_string());
        }
        if ui.button("Animations").clicked() {
            state.open_animations = true;
        }
        let Ok(doc) = designer.doc_mut() else {
            return;
        };
        let undo = ui
            .add_enabled(doc.undo_len() > 0, egui::Button::new("Undo"))
            .clicked();
        let redo = ui
            .add_enabled(doc.redo_len() > 0, egui::Button::new("Redo"))
            .clicked();
        let mut result = Ok(());
        if undo {
            result = doc.undo();
        } else if redo {
            result = doc.redo();
        }
        let save = ui.button(if doc.dirty() { "Save*" } else { "Save" }).clicked();
        if save {
            let target = (!state.open_path.trim().is_empty() && doc.path.is_none()).then(path);
            result = doc.save(target).map(|_| ());
        }
        if let Some(p) = &doc.path {
            ui.label(p.display().to_string());
        }
        if let Err(e) = result {
            designer.error = Some(e.to_string());
        }
    });
}

fn hierarchy(
    ui: &mut egui::Ui,
    doc: &mut UiDesign,
    state: &mut UiDesignerPanelState,
    clipboard: &mut Option<String>,
    error: &mut Option<String>,
) {
    ui.heading("Hierarchy");
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("ui-designer-preset")
            .selected_text(PRESETS[state.preset.min(PRESETS.len() - 1)])
            .show_ui(ui, |ui| {
                for (i, name) in PRESETS.iter().enumerate() {
                    ui.selectable_value(&mut state.preset, i, *name);
                }
            });
        let selected = doc.selection.clone().unwrap_or_default();
        if ui.button("Add").clicked()
            && let Some(node) = preset(PRESETS[state.preset.min(PRESETS.len() - 1)])
        {
            report(
                error,
                doc.apply(DesignOp::Add {
                    parent: selected.clone(),
                    index: None,
                    node: Box::new(node),
                }),
            );
        }
        let has_parent = !selected.is_empty();
        let many: Vec<NodePath> = doc
            .selected_nodes()
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect();
        if ui
            .add_enabled(has_parent, egui::Button::new("Duplicate"))
            .on_hover_text("Ctrl+D")
            .clicked()
        {
            report(error, doc.duplicate_many(&many));
        }
        if ui
            .add_enabled(has_parent, egui::Button::new("Remove"))
            .on_hover_text("Delete")
            .clicked()
        {
            report(error, doc.remove_many(&many));
        }
        if let Some((&last, parent)) = selected.split_last() {
            if ui.add_enabled(last > 0, egui::Button::new("Up")).clicked() {
                report(
                    error,
                    doc.apply(DesignOp::Move {
                        node: selected.clone(),
                        parent: parent.to_vec(),
                        index: Some(last - 1),
                    }),
                );
            }
            let siblings = node_at(&doc.file().root, parent).map_or(0, |p| p.children.len());
            if ui
                .add_enabled(last + 1 < siblings, egui::Button::new("Down"))
                .clicked()
            {
                report(
                    error,
                    doc.apply(DesignOp::Move {
                        node: selected.clone(),
                        parent: parent.to_vec(),
                        index: Some(last + 1),
                    }),
                );
            }
        }
    });
    ui.horizontal(|ui| {
        let many: Vec<NodePath> = doc
            .selected_nodes()
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect();
        let copy = ui
            .add_enabled(!many.is_empty(), egui::Button::new("Copy"))
            .on_hover_text("Ctrl+C: the nodes' JSON, kept across documents")
            .clicked();
        let cut = ui
            .add_enabled(!many.is_empty(), egui::Button::new("Cut"))
            .on_hover_text("Ctrl+X")
            .clicked();
        if (copy || cut)
            && let Some(text) = report_ok(error, copy_text(&doc.file().root, &many))
        {
            ui.ctx().copy_text(text.clone());
            *clipboard = Some(text);
            if cut {
                report(error, doc.remove_many(&many));
            }
        }
        if ui
            .add_enabled(clipboard.is_some(), egui::Button::new("Paste"))
            .on_hover_text("Ctrl+V: after the selection")
            .clicked()
            && let Some(text) = clipboard.clone()
        {
            let (parent, index) = paste_target(doc);
            report(error, doc.paste(&text, &parent, index));
        }
        if ui
            .add_enabled(many.len() > 1, egui::Button::new("Group"))
            .on_hover_text("Wrap the selected siblings in a new panel")
            .clicked()
        {
            report(error, doc.group(&many, None));
        }
    });
    palette(ui);
    if let Some(use_path) = component_palette(ui, doc, state) {
        let parent = doc.selection.clone().unwrap_or_default();
        report(
            error,
            doc.apply(DesignOp::Add {
                parent,
                index: None,
                node: Box::new(instance_node(&use_path)),
            }),
        );
    }
    let tree = tree_rows(doc, state);
    let mut dropped = None;
    #[cfg(test)]
    tests::TREE_ROWS.with(|rows| rows.borrow_mut().clear());
    let chosen = doc.selected_nodes();
    // An instance's row lists its inherited nodes, so rows vary in height: only the rows in view
    // are laid out (ADR-7326).
    let line = ui.spacing().interact_size.y;
    let spacing = ui.spacing().item_spacing.y;
    crate::virtual_list::variable_rows(
        ui,
        "ui-designer-tree",
        tree.len(),
        f32::INFINITY,
        |index| {
            let lines = 1 + tree[index].inherited.len();
            lines as f32 * line + (lines - 1) as f32 * spacing
        },
        |ui, index| {
            let TreeRow {
                path,
                label,
                inherited,
            } = &tree[index];
            let selected = chosen.contains(path);
            let row = ui
                .selectable_label(selected, label.as_str())
                .interact(egui::Sense::click_and_drag());
            #[cfg(test)]
            test_rect(ui, &path_string(path), row.rect);
            #[cfg(test)]
            tests::TREE_ROWS.with(|rows| rows.borrow_mut().push(path_string(path)));
            if !path.is_empty() {
                row.dnd_set_drag_payload(path.clone());
            }
            let zone = ui
                .input(|i| i.pointer.interact_pos())
                .map(|pos| DropZone::at((pos.y - row.rect.top()) / row.rect.height().max(1.0)));
            let hovering = row.dnd_hover_payload::<NodePath>().is_some()
                || row.dnd_hover_payload::<PaletteNode>().is_some()
                || row.dnd_hover_payload::<PaletteComponent>().is_some();
            if hovering && let Some(zone) = zone {
                drop_marker(ui, row.rect, zone);
            }
            if let (Some(dragged), Some(zone)) = (release_payload::<NodePath>(&row), zone) {
                dropped = drop_move(&dragged, path, zone);
            } else if let (Some(chip), Some(zone)) = (release_payload::<PaletteNode>(&row), zone) {
                dropped = preset(chip.0).map(|node| drop_add(node, path, zone));
            } else if let (Some(chip), Some(zone)) = (release_payload::<PaletteComponent>(&row), zone) {
                dropped = Some(drop_add(instance_node(&chip.0), path, zone));
            } else if row.clicked() {
                let modifiers = ui.input(|i| i.modifiers);
                if modifiers.command {
                    let _ = doc.toggle_selected(path.clone());
                } else if modifiers.shift {
                    // A range in document order from the primary selection.
                    let from = doc
                        .selection
                        .as_ref()
                        .and_then(|p| tree.iter().position(|o| o.path == *p));
                    let to = tree.iter().position(|o| o.path == *path);
                    if let (Some(a), Some(b)) = (from, to) {
                        let (lo, hi) = (a.min(b), a.max(b));
                        let mut range: Vec<NodePath> = tree[lo..=hi].iter().map(|o| o.path.clone()).collect();
                        // The primary stays first.
                        if a > b {
                            range.reverse();
                        }
                        let _ = doc.select_many(range);
                    }
                } else {
                    let _ = doc.select(Some(path.clone()));
                }
            }
            // An instance's own nodes come from its component: listed dimmed, not editable.
            for text in inherited {
                ui.add_enabled(false, egui::Label::new(text.as_str()));
            }
        },
    );
    if let Some(op) = dropped {
        report(error, doc.apply(op));
    }
}

/// The dropped payload if it is a `T`. `dnd_release_payload` takes the payload whatever its type, so
/// asking for the wrong type would lose it.
fn release_payload<T: std::any::Any + Send + Sync>(response: &egui::Response) -> Option<std::sync::Arc<T>> {
    egui::DragAndDrop::has_payload_of_type::<T>(&response.ctx)
        .then(|| response.dnd_release_payload::<T>())
        .flatten()
}

/// The palette: one chip per preset, dragged onto a hierarchy row or a preview node.
fn palette(ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        ui.label("Palette");
        for name in PRESETS {
            let chip = ui
                .selectable_label(false, name)
                .interact(egui::Sense::drag())
                .on_hover_text("Drag onto a row or the preview");
            #[cfg(test)]
            test_rect(ui, &format!("palette:{name}"), chip.rect);
            chip.dnd_set_drag_payload(PaletteNode(name));
        }
    });
}

/// Where a dragged row would land: a line above or below the row, or a box around it (a child).
fn drop_marker(ui: &egui::Ui, rect: egui::Rect, zone: DropZone) {
    let stroke = egui::Stroke::new(2.0, egui::Color32::from_rgb(232, 121, 58));
    let painter = ui.painter();
    match zone {
        DropZone::Before => {
            painter.hline(rect.x_range(), rect.top(), stroke);
        }
        DropZone::After => {
            painter.hline(rect.x_range(), rect.bottom(), stroke);
        }
        DropZone::Into => {
            painter.rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Inside);
        }
    }
}

/// The schematic look of one preview node: its authored fill, an outline (red past the safe
/// area) and its text.
fn schematic_node(
    painter: &egui::Painter,
    node: &crate::ui_designer_preview::PreviewNode,
    r: egui::Rect,
    fill: Option<&egui::Color32>,
    fit: f32,
) {
    if let Some(fill) = fill {
        painter.rect_filled(r, 0.0, *fill);
    }
    let color = if node.outside_safe_area {
        egui::Color32::from_rgb(230, 110, 90)
    } else {
        egui::Color32::from_gray(110)
    };
    painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, color), egui::StrokeKind::Inside);
    if let Some(text) = node.text.as_deref().filter(|t| !t.is_empty()) {
        painter.text(
            r.left_top() + egui::vec2(2.0, 1.0),
            egui::Align2::LEFT_TOP,
            text,
            egui::FontId::proportional((12.0 * fit / 0.5).clamp(8.0, 14.0)),
            egui::Color32::from_gray(220),
        );
    }
}

fn preview_view(
    ui: &mut egui::Ui,
    doc: &mut UiDesign,
    state: &mut UiDesignerPanelState,
    runtime: &mut crate::ui_designer_interactive::DesignerRuntime,
    project: Option<&std::path::Path>,
    error: &mut Option<String>,
) {
    ui.heading("Preview");
    ui.horizontal(|ui| {
        let current = SIZE_PRESETS
            .iter()
            .find(|(_, size, safe)| *size == doc.preview.size && *safe == doc.preview.safe_area)
            .map_or("custom", |(n, ..)| *n);
        egui::ComboBox::from_id_salt("ui-designer-size")
            .selected_text(current)
            .show_ui(ui, |ui| {
                for (name, ..) in SIZE_PRESETS {
                    if ui.selectable_label(name == current, name).clicked() {
                        let _ = doc.preview.use_preset(name);
                    }
                }
            });
        ui.checkbox(&mut doc.preview.pseudo, "Pseudo");
        ui.add_enabled_ui(!runtime.interactive, |ui| {
            ui.checkbox(&mut state.themed, "Themed")
                .on_hover_text("Draw with the runtime UI renderer and the project theme (GPU, offscreen)");
        });
        let mut locale = doc.preview.locale.clone().unwrap_or_default();
        if ui
            .add(
                egui::TextEdit::singleline(&mut locale)
                    .hint_text("locale")
                    .desired_width(60.0),
            )
            .changed()
        {
            doc.preview.locale = (!locale.trim().is_empty()).then(|| locale.trim().to_owned());
        }
        ui.label("items");
        ui.add(egui::DragValue::new(&mut doc.preview.repeat_count).range(0..=MAX_REPEAT_COUNT))
            .on_hover_text("Sample items each repeat node shows");
        if state.themed {
            theme_picker(ui, doc, state);
        }
    });
    size_rows(ui, doc, error);
    preview_scale_rows(ui, &mut doc.preview);
    if let Some(project) = project {
        doc.preview.assets_root = Some(project.join("assets"));
    }
    let scale_revision = crate::project_settings_ext::ui_scale_revision();
    if state.scale_revision != scale_revision {
        state.cached = None;
        state.themed_cache = None;
        state.scale_revision = scale_revision;
    }
    if runtime.interactive
        && state.fresh_input
        && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
    {
        runtime.stop();
    }
    canvas_toolbar(ui, state, runtime);
    if runtime.interactive {
        interactive_canvas(ui, doc, state, runtime, project, error);
        return;
    }

    // The layout variant switcher (P-271, ADR-7858).
    crate::ui_designer_variants::variant_bar(ui, doc, error);
    let stale = state
        .cached
        .as_ref()
        .is_none_or(|(rev, settings, _)| *rev != doc.revision() || *settings != doc.preview);
    if stale {
        let localization = project.and_then(project_localization);
        match preview(doc.file(), &doc.preview, localization) {
            Ok(frame) => state.cached = Some((doc.revision(), doc.preview.clone(), frame)),
            Err(e) => {
                ui.label(e.to_string());
                state.cached = None;
                return;
            }
        }
    }
    // The themed pixels, redrawn only when the document or the settings change.
    let themed_stale = state
        .themed_cache
        .as_ref()
        .is_none_or(|(rev, settings, _)| *rev != doc.revision() || *settings != doc.preview);
    if !state.themed {
        state.themed_cache = None;
        state.themed_error = None;
    } else if themed_stale && let Some((device, choice)) = &state.themed_inputs {
        let localization = project.and_then(project_localization);
        match themed_preview(doc.file(), &doc.preview, localization, choice, device) {
            Ok(frame) => {
                let texture = ui.ctx().load_texture(
                    "ui-designer-themed",
                    frame.to_color_image(),
                    egui::TextureOptions::LINEAR,
                );
                state.themed_cache = Some((doc.revision(), doc.preview.clone(), texture));
                state.themed_error = None;
            }
            Err(e) => {
                state.themed_cache = None;
                state.themed_error = Some(e.to_string());
            }
        }
    }
    if let Some(e) = &state.themed_error {
        ui.colored_label(egui::Color32::from_rgb(230, 110, 90), e);
    }
    // Pictures named by asset path (ADR-3148), by document path, and their textures.
    let mut pictures = std::collections::HashMap::new();
    walk(&doc.file().root, &mut Vec::new(), &mut |node, path| {
        if let Some(asset) = node.image.as_ref().and_then(|i| i.asset.as_ref()) {
            pictures.insert(path_string(path), asset.clone());
        }
    });
    if themed_off(state)
        && let Some(root) = project
    {
        for asset in pictures.values() {
            if state.picture_textures.contains_key(asset) {
                continue;
            }
            let texture = match state.picture_cache.get(root, asset).as_ref() {
                Ok((w, h, rgba)) => Some(ui.ctx().load_texture(
                    format!("ui-designer-picture:{asset}"),
                    egui::ColorImage::from_rgba_unmultiplied([*w as usize, *h as usize], rgba),
                    egui::TextureOptions::LINEAR,
                )),
                Err(_) => None,
            };
            state.picture_textures.insert(asset.clone(), texture);
        }
    }
    let Some((_, _, cached)) = &state.cached else {
        return;
    };
    let frame = cached.clone();
    ui.label(format!("Scale {:.2}x", frame.scale));
    let mut box_model = doc.preview.box_model;
    ui.checkbox(&mut box_model, "Margin and padding");
    if state.fresh_input {
        doc.preview.box_model = box_model;
    }
    let themed = state.themed_cache.as_ref().map(|(_, _, t)| t.id());
    let avail = [ui.available_width().max(64.0), 480.0];
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(avail[0], avail[1]), egui::Sense::click_and_drag());
    #[cfg(test)]
    test_rect(ui, "canvas", rect);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, egui::Color32::from_gray(16));
    // Wheel zooms about the pointer, a middle drag pans.
    if let Some(pointer) = response.hover_pos().filter(|_| state.fresh_input) {
        let wheel: f32 = ui.input(|i| {
            i.events
                .iter()
                .map(|e| match e {
                    egui::Event::MouseWheel { unit, delta, .. } => match unit {
                        egui::MouseWheelUnit::Point => delta.y,
                        egui::MouseWheelUnit::Line => delta.y * 40.0,
                        egui::MouseWheelUnit::Page => delta.y * 400.0,
                    },
                    _ => 0.0,
                })
                .sum()
        });
        if wheel != 0.0 {
            let at = pointer - rect.min;
            state
                .view
                .zoom_about(avail, frame.size, [at.x, at.y], (wheel / 240.0).exp());
        }
    }
    if state.fresh_input && response.dragged_by(egui::PointerButton::Middle) {
        let d = response.drag_delta();
        if state.view.zoom.is_none() {
            state.view.zoom = Some(state.view.scale(avail, frame.size));
        }
        state.view.pan[0] += d.x;
        state.view.pan[1] += d.y;
    }
    let k = state.view.scale(avail, frame.size);
    let origin = state.view.origin(avail, frame.size);
    let origin = rect.min + egui::vec2(origin[0], origin[1]);
    let to_screen = |r: [f32; 4]| {
        egui::Rect::from_min_size(
            origin + egui::vec2(r[0] * k, r[1] * k),
            egui::vec2(r[2] * k, r[3] * k),
        )
    };
    let screen = to_screen([0.0, 0.0, frame.size[0], frame.size[1]]);
    if !state.hide_device
        && let Some(device) = device_frame(frame.size, frame.safe_area)
    {
        let body = screen.expand(device.bezel * k);
        painter.rect_filled(body, device.corner * k, egui::Color32::from_gray(40));
        painter.rect_stroke(
            body,
            device.corner * k,
            egui::Stroke::new(1.0, egui::Color32::from_gray(90)),
            egui::StrokeKind::Outside,
        );
        painter.rect_filled(screen, 0.0, egui::Color32::from_gray(24));
        if let Some(cutout) = device.cutout {
            let c = to_screen(cutout);
            painter.circle_filled(c.center(), c.width() / 2.0, egui::Color32::BLACK);
        }
    } else {
        painter.rect_filled(screen, 0.0, egui::Color32::from_gray(24));
    }
    if let Some(texture) = themed {
        painter.image(
            texture,
            screen,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    }
    let [l, t, r, b] = frame.safe_area;
    let safe = to_screen([l, t, frame.size[0] - l - r, frame.size[1] - t - b]);
    painter.rect_stroke(
        safe,
        0.0,
        egui::Stroke::new(1.0, egui::Color32::from_rgb(90, 160, 90)),
        egui::StrokeKind::Inside,
    );
    let chosen: Vec<String> = doc.selected_nodes().iter().map(|p| path_string(p)).collect();
    // Background colors by document path: a repeat's rows share their template's.
    let mut fills = std::collections::HashMap::new();
    walk(&doc.file().root, &mut Vec::new(), &mut |node, path| {
        if let Some(BackgroundDef::Color(c)) = &node.background {
            let [r, g, b, a] = c.0;
            fills.insert(
                path_string(path),
                egui::Color32::from(egui::Rgba::from_rgba_unmultiplied(r, g, b, a)),
            );
        }
    });
    let accent = egui::Color32::from_rgb(232, 121, 58);
    for node in frame.nodes.iter().filter(|n| n.visible) {
        let r = to_screen(node.rect);
        #[cfg(test)]
        test_rect(ui, &format!("preview:{}", node.path), r);
        if themed.is_none() {
            schematic_node(&painter, node, r, fills.get(&node.path), k);
            if let Some(Some(texture)) = pictures
                .get(&node.path)
                .and_then(|a| state.picture_textures.get(a))
            {
                painter.image(
                    texture.id(),
                    r,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
        }
        if chosen.contains(&node.path) {
            painter.rect_stroke(r, 0.0, egui::Stroke::new(2.0, accent), egui::StrokeKind::Outside);
        }
    }
    if doc.preview.box_model {
        let _span = tracing::trace_span!("ui_designer_box_model").entered();
        let tokens = crate::theme::ui_tokens(ui.ctx());
        let colors = [
            tokens.warn.gamma_multiply(0.35),
            tokens.accents.ok.gamma_multiply(0.35),
        ];
        for node in frame
            .nodes
            .iter()
            .filter(|n| n.visible && chosen.contains(&n.path))
        {
            if let Some(bands) = &node.box_model_bands {
                for (bands, color) in bands.iter().zip(colors) {
                    for &band in bands {
                        painter.rect_filled(to_screen(band), 0.0, color);
                    }
                }
            }
        }
    }
    // Resize handles on a lone selected node.
    let primary = doc
        .selection
        .clone()
        .filter(|p| !p.is_empty() && chosen.len() == 1);
    let handles: Vec<(Handle, egui::Rect)> = primary
        .as_ref()
        .and_then(|p| rect_of(&frame, p))
        .map(|r| {
            Handle::RESIZE
                .iter()
                .map(|h| {
                    let [x, y] = h.anchor(r);
                    let c = origin + egui::vec2(x * k, y * k);
                    (*h, egui::Rect::from_center_size(c, egui::vec2(8.0, 8.0)))
                })
                .collect()
        })
        .unwrap_or_default();
    for (_handle, r) in &handles {
        #[cfg(test)]
        test_rect(ui, &format!("handle:{_handle:?}"), *r);
        painter.rect_filled(*r, 1.0, accent);
    }
    let hit = |root: &UiNodeDef, pos: egui::Pos2| {
        frame
            .nodes
            .iter()
            .rev()
            .find(|n| n.visible && to_screen(n.rect).contains(pos))
            .and_then(|n| crate::ui_designer::resolve_node(root, &n.path).ok())
    };
    // A drag on a handle resizes; on a node moves it (selecting it first).
    if response.drag_started_by(egui::PointerButton::Primary)
        && let Some(pos) = ui.input(|i| i.pointer.press_origin())
    {
        let grabbed = handles
            .iter()
            .find(|(_, r)| r.expand(2.0).contains(pos))
            .map(|(h, _)| (primary.clone().unwrap_or_default(), *h))
            .or_else(|| {
                let path = hit(&doc.file().root, pos)?;
                let modifiers = ui.input(|i| i.modifiers);
                if !chosen.contains(&path_string(&path)) && !modifiers.command && !modifiers.shift {
                    let _ = doc.select(Some(path.clone()));
                }
                Some((path, Handle::Body))
            })
            .filter(|(path, _)| !path.is_empty());
        state.drag = grabbed.and_then(|(node, handle)| {
            let start_rect = rect_of(&frame, &node)?;
            let selected = doc.selected_nodes();
            let group = (handle == Handle::Body && selected.len() > 1 && selected.contains(&node))
                .then_some(selected);
            // A group's live numbers are its bounding box's.
            let start_rect = group
                .as_ref()
                .and_then(|g| {
                    plan_group_move(&doc.file().root, &frame, g, [0.0, 0.0], &SnapSettings::OFF).ok()
                })
                .map_or(start_rect, |plan| plan.rect);
            Some(CanvasDrag {
                node,
                handle,
                start_rect,
                start_pointer: pos,
                // In a layout variant, a drag starts from that variant's values (ADR-7858).
                file: crate::ui_designer_variants::edited_document(doc.file(), &doc.preview).into_owned(),
                frame: frame.clone(),
                live: start_rect,
                pending: None,
                group,
                guides: Vec::new(),
            })
        });
    }
    if let Some(drag) = &mut state.drag
        && let Some(pos) = ui.input(|i| i.pointer.latest_pos())
    {
        let _span = tracing::info_span!("ui_designer_canvas_drag").entered();
        let delta = (pos - drag.start_pointer) / k;
        let modifiers = ui.input(|i| i.modifiers);
        let snap = if state.no_snap || modifiers.alt {
            SnapSettings::OFF
        } else {
            SnapSettings {
                grid: (state.grid > 0.0).then_some(state.grid),
                threshold: 6.0 / k,
                ..SnapSettings::ON
            }
        };
        if let Some(group) = drag.group.clone() {
            group_drag(doc, drag, &group, [delta.x, delta.y], &snap, error);
            state.guides.clone_from(&drag.guides);
        } else {
            let request = TransformRequest {
                node: &drag.node,
                rect: drag.handle.drag(drag.start_rect, [delta.x, delta.y]),
                mode: if drag.handle == Handle::Body {
                    TransformMode::Move
                } else {
                    TransformMode::Resize
                },
                snap,
                keep_aspect: modifiers.shift,
                reorder: true,
            };
            match plan_transform(&drag.file.root, &drag.frame, &request) {
                Ok(plan) => {
                    drag.live = plan.rect;
                    state.guides = plan.guides;
                    let start = node_at(&drag.file.root, &drag.node);
                    let flow_move = drag.handle == Handle::Body
                        && start.is_some_and(|n| n.style.position != PositionType::Absolute);
                    // Live: a style edit merged into one undo step; back at the start, the start style.
                    let op = plan.op.or_else(|| {
                        start.map(|n| DesignOp::Set {
                            node: drag.node.clone(),
                            key: "style".to_owned(),
                            value: crate::ui_designer::node_json(n)
                                .get("style")
                                .cloned()
                                .unwrap_or(Value::Null),
                        })
                    });
                    if flow_move {
                        drag.pending = op;
                    } else if let Some(op) = op {
                        // Nothing to write when the document already holds this style.
                        let current = node_at(&doc.file().root, &drag.node).map(|n| {
                            crate::ui_designer::node_json(n)
                                .get("style")
                                .cloned()
                                .unwrap_or(Value::Null)
                        });
                        let unchanged =
                            matches!(&op, DesignOp::Set { value, .. } if current.as_ref() == Some(value));
                        if !unchanged {
                            report(error, doc.apply_merged(op));
                        }
                    }
                }
                Err(e) => *error = Some(e.to_string()),
            }
        }
        let live = to_screen(drag.live);
        painter.rect_stroke(
            live,
            0.0,
            egui::Stroke::new(1.0, egui::Color32::WHITE),
            egui::StrokeKind::Outside,
        );
        let s = frame.scale.max(0.001);
        painter.text(
            live.left_top() - egui::vec2(0.0, 2.0),
            egui::Align2::LEFT_BOTTOM,
            format!(
                "{:.0}, {:.0}  {:.0} x {:.0}",
                (drag.live[0] - drag.start_rect[0]) / s,
                (drag.live[1] - drag.start_rect[1]) / s,
                drag.live[2] / s,
                drag.live[3] / s
            ),
            egui::FontId::monospace(11.0),
            egui::Color32::WHITE,
        );
    }
    for g in &state.guides {
        let stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(230, 70, 200));
        match g.axis {
            Axis::X => painter.vline(
                origin.x + g.at * k,
                (origin.y + g.span[0] * k)..=(origin.y + g.span[1] * k),
                stroke,
            ),
            Axis::Y => painter.hline(
                (origin.x + g.span[0] * k)..=(origin.x + g.span[1] * k),
                origin.y + g.at * k,
                stroke,
            ),
        };
    }
    if !ui.input(|i| i.pointer.primary_down())
        && let Some(drag) = state.drag.take()
    {
        state.guides.clear();
        if let Some(op) = drag.pending {
            report(error, doc.apply(op));
        }
    }
    if !state.hide_rulers {
        rulers(&painter, rect, origin, k, frame.size);
    }
    if state.show_nav {
        nav_overlay(&painter, &doc.file().root, &frame, &to_screen, ui.visuals());
    }
    if response.clicked()
        && let Some(pos) = response.interact_pointer_pos()
        && let Some(path) = hit(&doc.file().root, pos)
    {
        let modifiers = ui.input(|i| i.modifiers);
        if let Some(dir) = state.nav_pick.take() {
            // The armed Pick names the clicked node as the selected node's neighbor (ADR-7827).
            report(error, pick_neighbor(doc, dir, &path));
        } else if modifiers.command || modifiers.shift {
            let _ = doc.toggle_selected(path);
        } else {
            let _ = doc.select(Some(path));
        }
    }
    // A palette chip dropped on the preview lands on the topmost node under the pointer.
    let target = ui.input(|i| i.pointer.interact_pos()).and_then(|pos| {
        let node = frame
            .nodes
            .iter()
            .rev()
            .find(|n| n.visible && to_screen(n.rect).contains(pos))?;
        let path = crate::ui_designer::resolve_node(&doc.file().root, &node.path).ok()?;
        let rect = to_screen(node.rect);
        let zone = if path.is_empty() {
            DropZone::Into
        } else {
            DropZone::at((pos.y - rect.top()) / rect.height().max(1.0))
        };
        Some((path, rect, zone))
    });
    if response.dnd_hover_payload::<PaletteNode>().is_some()
        && let Some((_, rect, zone)) = &target
    {
        drop_marker(ui, *rect, *zone);
    }
    if let (Some(chip), Some((path, _, zone))) = (release_payload::<PaletteNode>(&response), &target)
        && let Some(node) = preset(chip.0)
    {
        report(error, doc.apply(drop_add(node, path, *zone)));
    }
    let outside = frame.nodes.iter().filter(|n| n.outside_safe_area).count();
    if outside > 0 {
        ui.label(format!("{outside} node(s) cross the safe area"));
    }
}

/// The canvas's view and snapping controls: Fit, 100 %, the zoom, Snap and its grid, rulers and
/// the device frame (panel state only).
fn canvas_toolbar(
    ui: &mut egui::Ui,
    state: &mut UiDesignerPanelState,
    runtime: &mut crate::ui_designer_interactive::DesignerRuntime,
) {
    ui.horizontal(|ui| {
        let mut playing = runtime.interactive;
        let play = ui
            .checkbox(&mut playing, "Play")
            .on_hover_text("Operate sample widgets; Esc returns to editing");
        #[cfg(test)]
        {
            test_rect(ui, "preview:play", play.rect);
            ui.data_mut(|d| d.insert_temp(egui::Id::new("preview-play-id"), play.id));
        }
        if play.changed() && state.fresh_input {
            state.live_texture = None;
            if playing {
                runtime.interactive = true;
            } else {
                runtime.stop();
            }
            state.drag = None;
            state.guides.clear();
        }

        let fit = ui.button("Fit").on_hover_text("Fit the frame into the panel");
        #[cfg(test)]
        test_rect(ui, "view:fit", fit.rect);
        if fit.clicked() {
            state.view.fit();
        }
        let actual = ui
            .button("100 %")
            .on_hover_text("One frame pixel per screen point");
        #[cfg(test)]
        test_rect(ui, "view:100", actual.rect);
        if actual.clicked() {
            state.view.actual_size();
        }
        match state.view.zoom {
            Some(z) => ui.label(format!("{:.0} %", z * 100.0)),
            None => ui.label("fit"),
        };
        let mut snap = !state.no_snap;
        if ui
            .checkbox(&mut snap, "Snap")
            .on_hover_text("Snap to the parent's edges, the siblings and the grid (Alt while dragging: off)")
            .changed()
        {
            state.no_snap = !snap;
        }
        ui.label("grid");
        ui.add(
            egui::DragValue::new(&mut state.grid)
                .range(0.0..=512.0)
                .speed(1.0),
        );
        let mut rulers = !state.hide_rulers;
        if ui.checkbox(&mut rulers, "Rulers").changed() {
            state.hide_rulers = !rulers;
        }
        let nav = ui
            .checkbox(&mut state.show_nav, "Show navigation")
            .on_hover_text("Arrows from each node to its declared focus neighbors");
        #[cfg(test)]
        test_rect(ui, "view:nav", nav.rect);
        let _ = nav;
        let mut device = !state.hide_device;
        if ui
            .checkbox(&mut device, "Device")
            .on_hover_text("A device frame around phone and handheld sizes")
            .changed()
        {
            state.hide_device = !device;
        }
    });
}

/// Rulers along the canvas's top and left edges, in frame px.
fn rulers(painter: &egui::Painter, rect: egui::Rect, origin: egui::Pos2, k: f32, size: [f32; 2]) {
    const W: f32 = 14.0;
    let step = ruler_step(k);
    let color = egui::Color32::from_gray(150);
    let font = egui::FontId::monospace(9.0);
    painter.rect_filled(
        egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), W)),
        0.0,
        egui::Color32::from_gray(32),
    );
    painter.rect_filled(
        egui::Rect::from_min_size(rect.min, egui::vec2(W, rect.height())),
        0.0,
        egui::Color32::from_gray(32),
    );
    for (axis, extent) in [(0usize, size[0]), (1, size[1])] {
        let mut v = 0.0;
        while v <= extent {
            let at = if axis == 0 {
                origin.x + v * k
            } else {
                origin.y + v * k
            };
            let (lo, hi) = if axis == 0 {
                (rect.left() + W, rect.right())
            } else {
                (rect.top() + W, rect.bottom())
            };
            if at >= lo && at <= hi {
                if axis == 0 {
                    painter.vline(at, rect.top()..=rect.top() + W, egui::Stroke::new(1.0, color));
                    painter.text(
                        egui::pos2(at + 2.0, rect.top()),
                        egui::Align2::LEFT_TOP,
                        format!("{v:.0}"),
                        font.clone(),
                        color,
                    );
                } else {
                    painter.hline(rect.left()..=rect.left() + W, at, egui::Stroke::new(1.0, color));
                    painter.text(
                        egui::pos2(rect.left() + 1.0, at + 1.0),
                        egui::Align2::LEFT_TOP,
                        format!("{v:.0}"),
                        font.clone(),
                        color,
                    );
                }
            }
            v += step;
        }
    }
}

/// Whether the schematic preview (not the themed one) shows this frame.
fn themed_off(state: &UiDesignerPanelState) -> bool {
    state.themed_cache.is_none()
}

fn inspector(
    ui: &mut egui::Ui,
    doc: &mut UiDesign,
    state: &mut UiDesignerPanelState,
    runtime: &mut crate::ui_designer_interactive::DesignerRuntime,
    models: Option<&ViewModels>,
    project: Option<&std::path::Path>,
    error: &mut Option<String>,
) {
    ui.heading("Inspector");
    egui::ScrollArea::vertical()
        .id_salt("ui-designer-inspector")
        .show(ui, |ui| {
            style_section(ui, doc, state, runtime, project, error);
            ui.add_enabled_ui(!runtime.interactive, |ui| {
                inspector_rows(ui, doc, state, models, project, error)
            });
        });
}

fn inspector_rows(
    ui: &mut egui::Ui,
    doc: &mut UiDesign,
    state: &mut UiDesignerPanelState,
    models: Option<&ViewModels>,
    project: Option<&std::path::Path>,
    error: &mut Option<String>,
) {
    let Some(path) = doc.selection.clone() else {
        ui.label("Select a node.");
        return;
    };
    let selected = doc.selected_nodes();
    if selected.len() > 1 {
        multi_rows(ui, doc, state, &selected, error);
        return;
    }
    // With a layout variant selected, the rows show and edit that variant's values (ADR-7858).
    let Some(node) = crate::ui_designer_variants::inspected_node(doc, &path) else {
        return;
    };
    ui.label(format!("{} {}", path_string(&path), node_kind(&node)));
    crate::ui_designer_variants::override_rows(ui, doc, &path, error);
    if path.is_empty() {
        screen_back_rows(ui, doc);
    }
    let set = |doc: &mut UiDesign, error: &mut Option<String>, key: &str, value: Value| {
        report(
            error,
            doc.apply(DesignOp::Set {
                node: path.clone(),
                key: key.to_owned(),
                value,
            }),
        );
    };
    text_row(ui, "name", node.name.clone().unwrap_or_default(), |v| {
        set(
            doc,
            error,
            "name",
            if v.is_empty() { Value::Null } else { json!(v) },
        );
    });
    text_row(ui, "classes", node.classes.join(" "), |v| {
        let classes: Vec<&str> = v.split_whitespace().collect();
        set(doc, error, "classes", json!(classes));
    });
    if node.use_component.is_some() {
        // An instance sets only its placement, parameters, bind root and slot content (spec 111 § 2.2).
        instance_rows(ui, doc, state, &path, &node, error);
        ui.separator();
        ui.label("Layout");
        anchor_rows(ui, doc, state, &path, error);
        for (key, value) in layout_rows(ui, &node) {
            report(
                error,
                doc.apply_merged(DesignOp::Set {
                    node: path.clone(),
                    key: key.to_owned(),
                    value,
                }),
            );
        }
        return;
    }
    if !path.is_empty() {
        make_component_rows(ui, doc, state, &path, &node, error);
    }
    // One plain span can be edited here; other text goes through the JSON row.
    let spans = node.text.as_ref().map_or(&[][..], |t| t.spans.as_slice());
    let plain = match spans {
        [] => Some((String::new(), String::new())),
        [TextSpanDef::Lit(s)] => Some((s.clone(), String::new())),
        [TextSpanDef::Loc(LocSpan::Loc(key))] => Some((String::new(), key.clone())),
        _ => None,
    };
    if let Some((literal, key)) = plain {
        // A localized node's text is read-only while a non-source locale previews (ADR-7855).
        if !key.is_empty() && crate::loc_panel::preview::is_previewing(ui.ctx()) {
            locked_text_row(ui, "text", &literal);
            locked_text_row(ui, "loc key", &key);
        } else {
            text_row(ui, "text", literal, |v| set(doc, error, "text", json!(v)));
            text_row(ui, "loc key", key, |v| {
                set(doc, error, "text.spans", json!([{ "Loc": v }]));
            });
        }
    }
    // `.ui.ron` v11 fold (ADR-7827): rich-text markup and what it renders as.
    if let Some(text) = &node.text {
        ui.horizontal(|ui| {
            let mut on = text.markup;
            let r = ui.checkbox(&mut on, "Markup").on_hover_text(
                "Read <b>, <i>, <u>, <s>, <color=#rrggbb>, <size=px> and <icon=name> in the text",
            );
            #[cfg(test)]
            test_rect(ui, "markup", r.rect);
            #[cfg(test)]
            test_id(ui, "markup", r.id);
            if r.changed() {
                set(
                    doc,
                    error,
                    "text.markup",
                    if on { json!(true) } else { Value::Null },
                );
            }
        });
        if text.markup {
            markup_preview(ui, text);
        }
    }
    text_row(ui, "command", node.command.clone().unwrap_or_default(), |v| {
        set(
            doc,
            error,
            "command",
            if v.is_empty() { Value::Null } else { json!(v) },
        );
    });
    if !path.is_empty() {
        nav_rows(ui, doc, state, &path, &node, error);
    }
    if let Some(widget) = &node.widget {
        ui.label(format!(
            "Widget: {}",
            crate::ui_designer_widget::variant_name(widget)
        ));
        for (key, value) in widget_rows(ui, widget) {
            // Merged: one drag on a number is one undo step.
            report(
                error,
                doc.apply_merged(DesignOp::Set {
                    node: path.clone(),
                    key: format!("widget.{key}"),
                    value,
                }),
            );
        }
    }
    if let Some(image) = &node.image {
        ui.horizontal(|ui| {
            ui.label("image tint");
            let mut rgba = image.tint.0;
            if ui.color_edit_button_rgba_unmultiplied(&mut rgba).changed() {
                set(
                    doc,
                    error,
                    "image.tint",
                    serde_json::to_value(UiColor(rgba)).unwrap_or(Value::Null),
                );
            }
            let mut keep = image.preserve_aspect;
            if ui.checkbox(&mut keep, "keep aspect").changed() {
                set(doc, error, "image.preserve_aspect", json!(keep));
            }
        });
        // `.ui.ron` v11 (ADR-9980): stretch, contain or cover, and tile (ADR-7751); "keep aspect"
        // leaves `fit` unwritten.
        ui.horizontal(|ui| {
            ui.label("image fit");
            let shown = image.fit.map_or("keep aspect", regia_ui::ImageFit::name);
            let mut chosen = image.fit;
            egui::ComboBox::from_id_salt("ui-designer-image-fit")
                .selected_text(shown)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut chosen, None, "keep aspect");
                    for fit in regia_ui::ImageFit::ALL {
                        ui.selectable_value(&mut chosen, Some(fit), fit.name());
                    }
                });
            if chosen != image.fit {
                let value = chosen.map_or(Value::Null, |fit| json!(fit.name()));
                set(doc, error, "image.fit", value);
            }
        });
    }
    let widget = node
        .widget
        .as_ref()
        .and_then(|w| serde_json::to_string(w).ok())
        .unwrap_or_default();
    egui::CollapsingHeader::new("Advanced (widget JSON)")
        .id_salt("ui-designer-widget-json")
        .default_open(false)
        .show(ui, |ui| {
            text_row(ui, "widget", widget, |v| {
                if v.trim().is_empty() {
                    set(doc, error, "widget", Value::Null);
                } else {
                    match serde_json::from_str::<Value>(&v) {
                        Ok(json) => set(doc, error, "widget", json),
                        Err(e) => *error = Some(format!("bad_value: {e}")),
                    }
                }
            });
        });
    ui.separator();
    ui.label("Layout");
    anchor_rows(ui, doc, state, &path, error);
    for (key, value) in layout_rows(ui, &node) {
        report(
            error,
            doc.apply_merged(DesignOp::Set {
                node: path.clone(),
                key: key.to_owned(),
                value,
            }),
        );
    }
    ui.separator();
    repeat_rows(ui, doc, &path, &node, models, error);
    ui.separator();
    ui.label("Bindings");
    if let Some(bind) = &node.bind {
        ui.label(format!(
            "bind: {} -> {} (single Tier 1 binding; JSON key `bind`)",
            bind.source,
            bind.target.name()
        ));
    }
    let mut bindings = node.bindings.clone();
    let mut changed = false;
    let mut remove = None;
    let item_fields = item_fields_at(&doc.file().root, &path, models).unwrap_or_default();
    for (i, bind) in bindings.iter_mut().enumerate() {
        ui.push_id(i, |ui| {
            ui.group(|ui| {
                // Where the binding reads from (ADR-7751): a field, a theme token or an animation.
                source_kind_row(ui, bind, &mut changed);
                if bind.theme.is_some() || bind.animation.is_some() {
                    dynamic_source_rows(ui, bind, &mut changed);
                } else {
                    text_row(ui, "source", bind.source.clone(), |v| {
                        bind.source = v;
                        changed = true;
                    });
                }
                if let Some(models) = models
                    .filter(|m| !m.models().is_empty() && bind.theme.is_none() && bind.animation.is_none())
                {
                    egui::ComboBox::from_id_salt("ui-designer-source")
                        .selected_text("Pick a field")
                        .show_ui(ui, |ui| {
                            for (model, field) in models
                                .models()
                                .iter()
                                .flat_map(|m| m.fields.iter().map(move |f| (m, f)))
                            {
                                let source = format!("{}.{}", model.name, field.name);
                                let label = format!("{source} ({})", field.kind);
                                if ui.selectable_label(bind.source == source, label).clicked() {
                                    bind.source = source;
                                    changed = true;
                                }
                            }
                            for field in &item_fields {
                                let source = format!(".{}", field.name);
                                let label = format!("{source} ({}, item)", field.kind);
                                if ui.selectable_label(bind.source == source, label).clicked() {
                                    bind.source = source;
                                    changed = true;
                                }
                            }
                        });
                }
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("ui-designer-target")
                        .selected_text(bind.target.name())
                        .show_ui(ui, |ui| {
                            for t in BindTargetDef::ALL {
                                changed |= ui.selectable_value(&mut bind.target, t, t.name()).changed();
                            }
                        });
                    egui::ComboBox::from_id_salt("ui-designer-mode")
                        .selected_text(bind.mode.name())
                        .show_ui(ui, |ui| {
                            for m in [BindMode::OneWay, BindMode::TwoWay, BindMode::OneTime] {
                                changed |= ui.selectable_value(&mut bind.mode, m, m.name()).changed();
                            }
                        });
                    changed |= ui.checkbox(&mut bind.invert, "invert").changed();
                    if crate::a11y_audit::glyph_button(ui, "x", "Remove the binding").clicked() {
                        remove = Some(i);
                    }
                });
                text_row(ui, "format", bind.format.clone().unwrap_or_default(), |v| {
                    bind.format = (!v.is_empty()).then_some(v);
                    changed = true;
                });
                converter_rows(ui, bind, &mut changed);
                // Preview data only: samples stay out of the document.
                let key = sample_key(&doc.file().root, &path, &bind.source);
                let sample = doc.preview.samples.get(&key).cloned().unwrap_or_default();
                text_row(ui, "sample", sample, |v| {
                    if v.is_empty() {
                        doc.preview.samples.remove(&key);
                    } else {
                        doc.preview.samples.insert(key.clone(), v);
                    }
                });
            });
        });
    }
    if let Some(i) = remove {
        bindings.remove(i);
        changed = true;
    }
    if ui.button("Add binding").clicked() {
        let source = models
            .and_then(|m| m.models().first())
            .and_then(|m| m.fields.first().map(|f| format!("{}.{}", m.name, f.name)))
            .unwrap_or_else(|| "Model.field".to_owned());
        bindings.push(BindDef {
            source,
            ..BindDef::default()
        });
        changed = true;
    }
    if changed && bindings != node.bindings {
        // Merged: one drag on a converter's number is one undo step.
        report(
            error,
            doc.apply_merged(DesignOp::Set {
                node: path.clone(),
                key: "bindings".to_owned(),
                value: serde_json::to_value(&bindings).unwrap_or(Value::Null),
            }),
        );
    }
    ui.separator();
    picture_rows(ui, doc, &path, &node, state, project, error);
    ui.separator();
    ui.label("Field (JSON)");
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.field_key)
                .hint_text("style.size")
                .desired_width(90.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut state.field_value)
                .hint_text("[200, \"Auto\"]")
                .desired_width(120.0),
        );
        if ui.button("Set").clicked() {
            match serde_json::from_str::<Value>(&state.field_value) {
                Ok(v) => set(doc, error, &state.field_key.clone(), v),
                Err(e) => *error = Some(format!("bad_value: {e}")),
            }
        }
    });
    ui.separator();
    let stale = state.lint.as_ref().is_none_or(|(rev, _)| *rev != doc.revision());
    if stale {
        state.lint = Some((doc.revision(), lint(doc.file(), models)));
    }
    if let Some((_, findings)) = &state.lint {
        ui.label(format!("Lint: {} finding(s)", findings.len()));
        for f in findings {
            let color = if f.severity == "error" {
                egui::Color32::from_rgb(230, 110, 90)
            } else {
                egui::Color32::from_rgb(220, 180, 90)
            };
            ui.colored_label(color, format!("{} {}: {}", f.node, f.code, f.message));
        }
    }
}

/// The typed rows of the widget's fields (ADR-9730): `(field, JSON value)` for each changed one.
fn widget_rows(ui: &mut egui::Ui, widget: &regia_ui::authoring::WidgetDef) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    egui::Grid::new("ui-designer-widget")
        .num_columns(2)
        .show(ui, |ui| {
            for field in widget_fields(widget) {
                let key = field.key;
                let current = widget_field(widget, key).unwrap_or(Value::Null);
                ui.label(key);
                let id = ui.id().with(("ui-designer-widget-field", key));
                match field.ty {
                    FieldType::Bool => {
                        let mut v = current.as_bool().unwrap_or(false);
                        if ui.checkbox(&mut v, "").changed() {
                            out.push((key, json!(v)));
                        }
                    }
                    FieldType::Number => {
                        let mut v = current.as_f64().unwrap_or(0.0);
                        let r = ui.add(egui::DragValue::new(&mut v).speed(0.05));
                        #[cfg(test)]
                        test_rect(ui, &format!("widget:{key}"), r.rect);
                        if r.changed() {
                            out.push((key, json!(v)));
                        }
                    }
                    FieldType::Integer => {
                        let mut v = current.as_u64().unwrap_or(0);
                        if ui
                            .add(
                                egui::DragValue::new(&mut v)
                                    .range(0..=u64::from(u32::MAX))
                                    .speed(1.0),
                            )
                            .changed()
                        {
                            out.push((key, json!(v)));
                        }
                    }
                    FieldType::OptionalIndex => {
                        ui.horizontal(|ui| {
                            let mut on = !current.is_null();
                            let mut v = current.as_u64().unwrap_or(0);
                            let a = ui.checkbox(&mut on, "").changed();
                            let b = on
                                && ui
                                    .add(egui::DragValue::new(&mut v).range(0..=1024).speed(1.0))
                                    .changed();
                            if a || b {
                                out.push((key, if on { json!(v) } else { Value::Null }));
                            }
                        });
                    }
                    FieldType::Rect4 => {
                        ui.horizontal(|ui| {
                            let mut on = !current.is_null();
                            let mut v: [f64; 4] = serde_json::from_value(current.clone()).unwrap_or([0.0; 4]);
                            let mut changed = ui.checkbox(&mut on, "").changed();
                            if on {
                                for x in &mut v {
                                    changed |= ui.add(egui::DragValue::new(x).speed(1.0)).changed();
                                }
                            }
                            if changed {
                                out.push((key, if on { json!(v) } else { Value::Null }));
                            }
                        });
                    }
                    FieldType::Choice(words) => {
                        let before = current.as_str().unwrap_or_default().to_owned();
                        let mut chosen = before.clone();
                        egui::ComboBox::from_id_salt(id)
                            .selected_text(chosen.clone())
                            .show_ui(ui, |ui| {
                                for word in words {
                                    ui.selectable_value(&mut chosen, (*word).to_owned(), *word);
                                }
                            });
                        if chosen != before {
                            out.push((key, json!(chosen)));
                        }
                    }
                    FieldType::OptionalText => {
                        let shown = current.as_str().unwrap_or_default().to_owned();
                        let mut changed = None;
                        ui.push_id(id, |ui| {
                            text_row(ui, key, shown, |v| changed = Some(v));
                        });
                        if let Some(v) = changed {
                            // Empty clears it.
                            let v = v.trim().to_owned();
                            out.push((key, if v.is_empty() { Value::Null } else { json!(v) }));
                        }
                    }
                    FieldType::Char => {
                        let shown = current.as_str().unwrap_or_default().to_owned();
                        let mut changed = None;
                        ui.push_id(id, |ui| {
                            text_row(ui, key, shown, |v| changed = Some(v));
                        });
                        if let Some(v) = changed {
                            // Empty clears the mask; otherwise the first character is the mask.
                            let value = v.chars().next().map_or(Value::Null, |c| json!(c.to_string()));
                            out.push((key, value));
                        }
                    }
                    FieldType::Text | FieldType::TextList | FieldType::Json => {
                        let shown = match (field.ty, &current) {
                            (FieldType::Text, Value::String(t)) => t.clone(),
                            (FieldType::TextList, Value::Array(items)) => items
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(", "),
                            (_, Value::Null) => String::new(),
                            (_, v) => v.to_string(),
                        };
                        let mut changed = None;
                        ui.push_id(id, |ui| {
                            text_row(ui, key, shown, |v| changed = Some(v));
                        });
                        if let Some(v) = changed {
                            let value = match field.ty {
                                FieldType::Text => Some(json!(v)),
                                FieldType::TextList => Some(json!(
                                    v.split(',')
                                        .map(str::trim)
                                        .filter(|t| !t.is_empty())
                                        .collect::<Vec<_>>()
                                )),
                                _ => serde_json::from_str::<Value>(&v).ok(),
                            };
                            out.extend(value.map(|v| (key, v)));
                        }
                    }
                }
                ui.end_row();
            }
        });
    out
}

/// The inspector for several selected nodes (ADR-9730): the shared layout rows (a change goes to
/// every node as one undo step; mixed fields are listed), align, distribute, group, duplicate and
/// delete.
fn multi_rows(
    ui: &mut egui::Ui,
    doc: &mut UiDesign,
    state: &mut UiDesignerPanelState,
    selected: &[NodePath],
    error: &mut Option<String>,
) {
    ui.label(format!("{} nodes selected", selected.len()));
    let nodes: Vec<NodePath> = selected.iter().filter(|p| !p.is_empty()).cloned().collect();
    let frame = state.cached.as_ref().map(|(_, _, f)| f.clone());
    ui.horizontal_wrapped(|ui| {
        for kind in AlignKind::ALL {
            let label = match kind {
                AlignKind::Left => "Left",
                AlignKind::Center => "Center",
                AlignKind::Right => "Right",
                AlignKind::Top => "Top",
                AlignKind::Middle => "Middle",
                AlignKind::Bottom => "Bottom",
                AlignKind::DistributeH => "Distribute H",
                AlignKind::DistributeV => "Distribute V",
            };
            let need = if matches!(kind, AlignKind::DistributeH | AlignKind::DistributeV) {
                3
            } else {
                2
            };
            let button = ui.add_enabled(nodes.len() >= need && frame.is_some(), egui::Button::new(label));
            #[cfg(test)]
            test_rect(ui, &format!("align:{}", kind.name()), button.rect);
            if button.clicked()
                && let Some(frame) = &frame
            {
                report(error, doc.align(frame, &nodes, kind));
            }
        }
    });
    ui.horizontal(|ui| {
        if ui.button("Group").clicked() {
            report(error, doc.group(&nodes, None));
        }
        if ui.button("Duplicate").clicked() {
            report(error, doc.duplicate_many(&nodes));
        }
        if ui.button("Delete").clicked() {
            report(error, doc.remove_many(&nodes));
        }
    });
    let Some(primary) = nodes.first().and_then(|p| node_at(&doc.file().root, p)).cloned() else {
        return;
    };
    let (_, mixed) = shared_fields(&doc.file().root, &nodes);
    if !mixed.is_empty() {
        ui.label(format!(
            "mixed (the first node's value shown): {}",
            mixed.join(", ")
        ));
    }
    ui.separator();
    ui.label("Layout (every selected node)");
    for (key, value) in layout_rows(ui, &primary) {
        report(error, doc.set_many(&nodes, key, &value, true));
    }
}

/// The node's picture and mask (ADR-3148): a picker of the project's pictures and the icon atlas's
/// icons, the name as text, nine-slice insets and the mask fit. Each change is one `image` or `mask`
/// edit (one undo step; a drag on a nine-slice inset is one step however many frames it runs); the
/// schematic and the themed preview both show the picked picture.
fn picture_rows(
    ui: &mut egui::Ui,
    doc: &mut UiDesign,
    path: &[usize],
    node: &UiNodeDef,
    state: &mut UiDesignerPanelState,
    project: Option<&std::path::Path>,
    error: &mut Option<String>,
) {
    use crate::ui_designer_images::{edit_image, edit_mask, project_pictures};
    ui.horizontal(|ui| {
        ui.label("Picture");
        if ui
            .small_button("Refresh")
            .on_hover_text("Read the project's pictures again")
            .clicked()
        {
            state.pictures = None;
            state.picture_cache.clear();
            state.picture_textures.clear();
        }
    });
    if state.pictures.is_none() {
        state.pictures = Some(project.map(project_pictures).unwrap_or_default());
    }
    let pictures = state.pictures.clone().unwrap_or_default();
    let icons = state.icons.1.clone();
    let mut image_params: Option<Value> = None;
    // A nine-slice inset drag reports `changed` every frame: merge it into one undo step.
    let mut image_merged = false;
    let mut mask_params: Option<Value> = None;
    for (field, current) in [
        (
            "image",
            node.image.as_ref().map(|i| (i.asset.clone(), i.icon.clone())),
        ),
        (
            "mask",
            node.mask.as_ref().map(|m| (m.asset.clone(), m.icon.clone())),
        ),
    ] {
        let (asset, icon) = current.clone().unwrap_or_default();
        let out = if field == "image" {
            &mut image_params
        } else {
            &mut mask_params
        };
        ui.push_id(field, |ui| {
            ui.horizontal(|ui| {
                ui.label(field);
                egui::ComboBox::from_id_salt("ui-designer-picture")
                    .selected_text(asset.as_deref().unwrap_or("Pick a picture"))
                    .show_ui(ui, |ui| {
                        // A screenful at a time (ADR-7306): up to MAX_PICTURES entries.
                        crate::virtual_list::rows(
                            ui,
                            "ui-designer-picture-rows",
                            pictures.len(),
                            crate::virtual_list::DROP_DOWN_HEIGHT,
                            |ui, i| {
                                let p = &pictures[i];
                                if ui
                                    .selectable_label(asset.as_deref() == Some(p.as_str()), p.as_str())
                                    .clicked()
                                {
                                    *out = Some(json!({ "asset": p }));
                                }
                            },
                        );
                        if pictures.is_empty() {
                            ui.label("No pictures under assets/");
                        }
                    });
                if !icons.is_empty() {
                    egui::ComboBox::from_id_salt("ui-designer-icon")
                        .selected_text(icon.as_deref().unwrap_or("Icon"))
                        .show_ui(ui, |ui| {
                            for n in &icons {
                                if ui
                                    .selectable_label(icon.as_deref() == Some(n.as_str()), n.as_str())
                                    .clicked()
                                {
                                    *out = Some(json!({ "icon": n }));
                                }
                            }
                        });
                }
                if current.is_some()
                    && crate::a11y_audit::glyph_button(ui, "x", &format!("Remove the {field}")).clicked()
                {
                    *out = Some(json!({ "remove": true }));
                }
            });
            text_row(
                ui,
                &format!("{field} asset"),
                asset.clone().unwrap_or_default(),
                |v| {
                    *out = Some(if v.trim().is_empty() {
                        json!({ "remove": true })
                    } else {
                        json!({ "asset": v })
                    });
                },
            );
            text_row(
                ui,
                &format!("{field} icon"),
                icon.clone().unwrap_or_default(),
                |v| {
                    *out = Some(if v.trim().is_empty() {
                        json!({ "remove": true })
                    } else {
                        json!({ "icon": v })
                    });
                },
            );
        });
    }
    if let Some(image) = &node.image {
        let mut on = image.nine_slice.is_some();
        let (mut l, mut t, mut r, mut b) = image.nine_slice.unwrap_or((8.0, 8.0, 8.0, 8.0));
        ui.horizontal(|ui| {
            let mut changed = ui.checkbox(&mut on, "nine-slice").changed();
            if on {
                for v in [&mut l, &mut t, &mut r, &mut b] {
                    let response = ui.add(egui::DragValue::new(v).range(0.0..=4096.0).speed(0.5));
                    // The last inset (bottom) wins the key: tests drag that one.
                    #[cfg(test)]
                    test_rect(ui, "nine-slice inset", response.rect);
                    changed |= response.changed();
                }
            }
            if changed {
                image_params = Some(json!({ "nine_slice": on.then_some([l, t, r, b]) }));
                image_merged = true;
            }
        });
    }
    if let Some(mask) = &node.mask {
        ui.horizontal(|ui| {
            ui.label("mask fit");
            for fit in [
                regia_ui::MaskFit::Stretch,
                regia_ui::MaskFit::Contain,
                regia_ui::MaskFit::Cover,
                regia_ui::MaskFit::Tile,
            ] {
                let name = serde_json::to_value(fit)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default();
                if ui.selectable_label(mask.fit == fit, name.as_str()).clicked() && mask.fit != fit {
                    mask_params = Some(json!({ "fit": name }));
                }
            }
        });
    }
    if let Some(params) = image_params {
        let value =
            edit_image(node.image.as_ref(), &params).map(|d| serde_json::to_value(d).unwrap_or(Value::Null));
        report(
            error,
            value.and_then(|value| {
                let op = DesignOp::Set {
                    node: path.to_vec(),
                    key: "image".to_owned(),
                    value,
                };
                if image_merged {
                    doc.apply_merged(op)
                } else {
                    doc.apply(op)
                }
            }),
        );
    }
    if let Some(params) = mask_params {
        let value =
            edit_mask(node.mask.as_ref(), &params).map(|d| serde_json::to_value(d).unwrap_or(Value::Null));
        report(
            error,
            value.and_then(|value| {
                doc.apply(DesignOp::Set {
                    node: path.to_vec(),
                    key: "mask".to_owned(),
                    value,
                })
            }),
        );
    }
}

/// The node's repeat: source (with a picker of the model's list fields), key, and the fields of the
/// item in the hint.
fn repeat_rows(
    ui: &mut egui::Ui,
    doc: &mut UiDesign,
    path: &[usize],
    node: &UiNodeDef,
    models: Option<&ViewModels>,
    error: &mut Option<String>,
) {
    ui.label("Repeat");
    let source = node.repeat.as_ref().map_or(String::new(), |r| r.source.clone());
    let row_height = node
        .repeat
        .as_ref()
        .map_or(RepeatRowHeight::List, |r| r.row_height);
    let key = node
        .repeat
        .as_ref()
        .and_then(|r| r.key.clone())
        .unwrap_or_default();
    text_row(ui, "repeat source", source.clone(), |v| {
        report(
            error,
            doc.apply(repeat_op_with(path.to_vec(), Some(&v), Some(&key), row_height)),
        );
    });
    if !source.is_empty() {
        text_row(ui, "repeat key", key.clone(), |v| {
            report(
                error,
                doc.apply(repeat_op_with(path.to_vec(), Some(&source), Some(&v), row_height)),
            );
        });
        // A virtualized repeat's row height (ADR-7751): the list's, or the template's fixed height.
        if matches!(node.widget, Some(regia_ui::authoring::WidgetDef::List(..))) {
            ui.horizontal(|ui| {
                ui.label("row height");
                let mut chosen = row_height;
                egui::ComboBox::from_id_salt("ui-designer-repeat-row-height")
                    .selected_text(chosen.name())
                    .show_ui(ui, |ui| {
                        for mode in [RepeatRowHeight::List, RepeatRowHeight::Template] {
                            ui.selectable_value(&mut chosen, mode, mode.name());
                        }
                    });
                if chosen != row_height {
                    report(
                        error,
                        doc.apply(repeat_op_with(path.to_vec(), Some(&source), Some(&key), chosen)),
                    );
                }
                if chosen == RepeatRowHeight::Template {
                    let height = regia_ui::authoring::RepeatDef::template_row_height(&node.children);
                    ui.label(
                        height.map_or("the first child needs a fixed height".to_owned(), |h| {
                            format!("{h} px")
                        }),
                    );
                }
            });
        }
        if node.children.is_empty() {
            ui.label("Add a child: it is the template spawned per item.");
        }
    }
    let lists: Vec<String> = models.map_or(Vec::new(), |m| {
        m.models()
            .iter()
            .flat_map(|model| {
                model
                    .fields
                    .iter()
                    .filter(|f| f.kind == "list")
                    .map(move |f| format!("{}.{}", model.name, f.name))
            })
            .collect()
    });
    if !lists.is_empty() {
        egui::ComboBox::from_id_salt("ui-designer-repeat-source")
            .selected_text("Pick a list")
            .show_ui(ui, |ui| {
                for list in &lists {
                    if ui.selectable_label(*list == source, list).clicked() {
                        report(
                            error,
                            doc.apply(repeat_op_with(path.to_vec(), Some(list), Some(&key), row_height)),
                        );
                    }
                }
            });
    }
    let in_template = path
        .split_last()
        .is_some_and(|(_, parent)| !enclosing_repeats(&doc.file().root, parent).is_empty());
    if in_template {
        ui.label("Inside a repeat template: bind fields of the item as .field");
    }
}

/// The source-kind picker of one binding (ADR-7751): `field` (a model or item field), `theme` (a
/// token of the active theme) or `animation` (a number over time). Switching clears the others.
fn source_kind_row(ui: &mut egui::Ui, bind: &mut BindDef, changed: &mut bool) {
    let current = match (&bind.theme, &bind.animation) {
        (Some(_), _) => "theme",
        (None, Some(_)) => "animation",
        (None, None) => "field",
    };
    ui.horizontal(|ui| {
        ui.label("reads");
        egui::ComboBox::from_id_salt("ui-designer-source-kind")
            .selected_text(current)
            .show_ui(ui, |ui| {
                for kind in ["field", "theme", "animation"] {
                    if ui.selectable_label(current == kind, kind).clicked() && current != kind {
                        bind.theme = (kind == "theme").then(|| "color.accent".to_owned());
                        bind.animation = (kind == "animation").then(AnimationBindDef::default);
                        if kind == "field" {
                            bind.source = "Model.field".to_owned();
                        } else {
                            bind.source.clear();
                        }
                        *changed = true;
                    }
                }
            });
    });
}

/// The rows of a theme-token or animation binding (ADR-7751): the token, or `from`, `to`,
/// `duration`, `delay`, the ease and the repeat mode.
fn dynamic_source_rows(ui: &mut egui::Ui, bind: &mut BindDef, changed: &mut bool) {
    if let Some(token) = &mut bind.theme {
        text_row(ui, "theme token", token.clone(), |v| {
            *token = v;
            *changed = true;
        });
        return;
    }
    let Some(animation) = &mut bind.animation else {
        return;
    };
    ui.horizontal(|ui| {
        for (label, value) in [
            ("from", &mut animation.from),
            ("to", &mut animation.to),
            ("duration", &mut animation.duration),
            ("delay", &mut animation.delay),
        ] {
            ui.label(label);
            *changed |= ui.add(egui::DragValue::new(value).speed(0.05)).changed();
        }
    });
    ui.horizontal(|ui| {
        let ease = animation.ease.clone().unwrap_or_else(|| "linear".to_owned());
        egui::ComboBox::from_id_salt("ui-designer-animation-ease")
            .selected_text(ease.as_str())
            .show_ui(ui, |ui| {
                for (name, _) in regia_ui::tween::Ease::NAMED {
                    if ui.selectable_label(ease == name, name).clicked() && ease != name {
                        animation.ease = (name != "linear").then(|| name.to_owned());
                        *changed = true;
                    }
                }
            });
        egui::ComboBox::from_id_salt("ui-designer-animation-repeat")
            .selected_text(animation.repeat.name())
            .show_ui(ui, |ui| {
                for mode in regia_ui::AnimationRepeat::ALL {
                    *changed |= ui
                        .selectable_value(&mut animation.repeat, mode, mode.name())
                        .changed();
                }
            });
    });
}

/// The converter picker of one binding and its settings.
fn converter_rows(ui: &mut egui::Ui, bind: &mut BindDef, changed: &mut bool) {
    const NONE: &str = "none";
    let current = bind.converter.as_ref().map_or(NONE, BindConverter::name);
    ui.horizontal(|ui| {
        ui.label("converter");
        egui::ComboBox::from_id_salt("ui-designer-converter")
            .selected_text(current)
            .show_ui(ui, |ui| {
                if ui.selectable_label(current == NONE, NONE).clicked() && current != NONE {
                    bind.converter = None;
                    *changed = true;
                }
                for name in BindConverter::NAMES {
                    if ui.selectable_label(current == name, name).clicked() && current != name {
                        bind.converter = BindConverter::template(name);
                        *changed = true;
                    }
                }
            });
    });
    let drag = |ui: &mut egui::Ui, label: &str, value: &mut f64, changed: &mut bool| {
        ui.label(label);
        *changed |= ui.add(egui::DragValue::new(&mut *value).speed(0.1)).changed();
    };
    match &mut bind.converter {
        None | Some(BindConverter::BoolToVisibility) => {}
        Some(BindConverter::Number { precision, group }) => {
            ui.horizontal(|ui| {
                ui.label("precision");
                *changed |= ui
                    .add(egui::DragValue::new(precision).range(0..=regia_ui::converter::MAX_NUMBER_PRECISION))
                    .changed();
            });
            text_row(ui, "group", group.clone().unwrap_or_default(), |v| {
                *group = (!v.is_empty()).then_some(v);
                *changed = true;
            });
        }
        Some(BindConverter::Clamp { min, max }) => {
            ui.horizontal(|ui| {
                drag(ui, "min", min, changed);
                drag(ui, "max", max, changed);
            });
        }
        Some(BindConverter::MapRange { from, to, clamp }) => {
            ui.horizontal(|ui| {
                drag(ui, "from", &mut from.0, changed);
                drag(ui, "..", &mut from.1, changed);
            });
            ui.horizontal(|ui| {
                drag(ui, "to", &mut to.0, changed);
                drag(ui, "..", &mut to.1, changed);
                *changed |= ui.checkbox(clamp, "clamp").changed();
            });
        }
        Some(BindConverter::EnumText { table, default }) => {
            text_row(ui, "table", format_table(table), |v| {
                *table = parse_table(&v);
                *changed = true;
            });
            text_row(ui, "default", default.clone().unwrap_or_default(), |v| {
                *default = (!v.is_empty()).then_some(v);
                *changed = true;
            });
        }
        Some(BindConverter::LocKey { prefix }) => {
            text_row(ui, "prefix", prefix.clone(), |v| {
                *prefix = v;
                *changed = true;
            });
        }
    }
}

/// An `enum_text` table as text: `Idle=Waiting; Run=On the way`.
fn format_table(table: &[(String, String)]) -> String {
    table
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The text form of an `enum_text` table back to its entries; a part without `=` is dropped.
fn parse_table(text: &str) -> Vec<(String, String)> {
    text.split(';')
        .filter_map(|part| part.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

/// A label + text field that calls `apply` with the new text when editing ends.
/// The Navigation section (ADR-7827): one row per direction naming the neighbor (type a node name,
/// or arm Pick and click a node in the preview); an empty row clears it.
fn nav_rows(
    ui: &mut egui::Ui,
    doc: &mut UiDesign,
    state: &mut UiDesignerPanelState,
    path: &NodePath,
    node: &UiNodeDef,
    error: &mut Option<String>,
) {
    egui::CollapsingHeader::new("Navigation")
        .id_salt("ui-designer-nav")
        .default_open(!node.nav.is_empty())
        .show(ui, |ui| {
            for (dir, current) in node.nav.entries() {
                ui.horizontal(|ui| {
                    let label = format!("nav {dir}");
                    text_row(ui, &label, current.unwrap_or_default().to_owned(), |v| {
                        let v = v.trim().to_owned();
                        report(
                            error,
                            doc.apply(DesignOp::Set {
                                node: path.clone(),
                                key: format!("nav.{dir}"),
                                value: if v.is_empty() { Value::Null } else { json!(v) },
                            }),
                        );
                    });
                    let armed = state.nav_pick == Some(dir);
                    let pick = ui.selectable_label(armed, "Pick").on_hover_text(format!(
                        "Click a node in the preview to make it the {dir} neighbor"
                    ));
                    #[cfg(test)]
                    test_rect(ui, &format!("nav-pick:{dir}"), pick.rect);
                    if pick.clicked() {
                        state.nav_pick = if armed { None } else { Some(dir) };
                    }
                });
            }
            if !node.nav.is_empty() && !crate::ui_designer::focusable(node) {
                ui.label("Neighbors apply to focusable nodes only.");
            }
        });
}

/// Names the node at `target` as the `dir` neighbor of the selected node.
fn pick_neighbor(doc: &mut UiDesign, dir: &str, target: &[usize]) -> Result<NodePath, DesignerError> {
    let Some(source) = doc.selection.clone() else {
        return Err(DesignerError::new(
            "bad_node",
            "select the node whose neighbor to set",
        ));
    };
    if source.as_slice() == target {
        return Err(DesignerError::new("bad_value", "a node is not its own neighbor"));
    }
    let name = node_at(&doc.file().root, target)
        .and_then(|n| n.name.clone())
        .ok_or_else(|| {
            DesignerError::new(
                "bad_value",
                format!(
                    "the node at {} has no name; neighbors are stored by name, so name it first",
                    path_string(target)
                ),
            )
        })?;
    doc.apply(DesignOp::Set {
        node: source,
        key: format!("nav.{dir}"),
        value: json!(name),
    })
}

/// Arrows from each node to its declared focus neighbors (the canvas's Show navigation), in the
/// selection color; a neighbor name that matches no node draws a short arrow in the error color.
fn nav_overlay(
    painter: &egui::Painter,
    root: &UiNodeDef,
    frame: &PreviewFrame,
    to_screen: &dyn Fn([f32; 4]) -> egui::Rect,
    visuals: &egui::Visuals,
) {
    let ok = egui::Stroke::new(2.0, visuals.selection.stroke.color);
    let bad = egui::Stroke::new(2.0, visuals.error_fg_color);
    walk(root, &mut Vec::new(), &mut |node, path| {
        if node.nav.is_empty() {
            return;
        }
        let Some(from) = crate::ui_designer_canvas::rect_of(frame, path).map(to_screen) else {
            return;
        };
        for (dir, target) in node.nav.entries() {
            let Some(target) = target else {
                continue;
            };
            let to = crate::ui_designer::resolve_node(root, &format!("#{target}"))
                .ok()
                .and_then(|p| crate::ui_designer_canvas::rect_of(frame, &p))
                .map(to_screen);
            let start = from.center();
            match to {
                Some(to) => painter.arrow(start, to.center() - start, ok),
                None => {
                    let unit = match dir {
                        "up" => egui::vec2(0.0, -1.0),
                        "down" => egui::vec2(0.0, 1.0),
                        "left" => egui::vec2(-1.0, 0.0),
                        _ => egui::vec2(1.0, 0.0),
                    };
                    painter.arrow(start, unit * 24.0, bad);
                }
            }
        }
    });
}

/// What markup text renders as (ADR-7827): the spans of [`regia_ui::authoring::text_def_spans`]
/// as egui text, emphasis from the panel theme, the markup's own colors and sizes kept.
fn markup_preview(ui: &mut egui::Ui, text: &regia_ui::authoring::TextDef) {
    let spans = regia_ui::authoring::text_def_spans(text);
    let base_px = text.style.font_px.max(1.0);
    let body = egui::TextStyle::Body.resolve(ui.style()).size;
    let mut job = egui::text::LayoutJob::default();
    for span in &spans {
        let shown = match &span.content {
            regia_ui::text::TextContent::Lit(s) => s.clone(),
            regia_ui::text::TextContent::Loc { key, .. } => format!("\u{27E6}{key}\u{27E7}"),
        };
        let s = &span.style;
        let color = if s.color == text.style.color {
            if s.bold {
                ui.visuals().strong_text_color()
            } else {
                ui.visuals().text_color()
            }
        } else {
            let [r, g, b, a] = s.color.0;
            egui::Color32::from(egui::Rgba::from_rgba_unmultiplied(r, g, b, a))
        };
        let size = (body * s.font_px / base_px).clamp(6.0, 64.0);
        job.append(
            &shown,
            0.0,
            egui::TextFormat {
                font_id: egui::FontId::proportional(size),
                color,
                italics: s.italic,
                underline: if s.underline {
                    egui::Stroke::new(1.0, color)
                } else {
                    egui::Stroke::NONE
                },
                strikethrough: if s.strikethrough {
                    egui::Stroke::new(1.0, color)
                } else {
                    egui::Stroke::NONE
                },
                ..Default::default()
            },
        );
    }
    let shown = ui.label(job);
    #[cfg(test)]
    test_rect(ui, "markup-preview", shown.rect);
    let _ = shown;
}

fn text_row(ui: &mut egui::Ui, label: &str, value: String, apply: impl FnOnce(String)) {
    ui.horizontal(|ui| {
        ui.label(label);
        let id = ui.id().with(("ui-designer-field", label));
        let mut text = ui
            .data_mut(|d| d.get_temp::<String>(id))
            .unwrap_or_else(|| value.clone());
        let response = ui.add(egui::TextEdit::singleline(&mut text).desired_width(160.0));
        #[cfg(test)]
        test_rect(ui, label, response.rect);
        #[cfg(test)]
        test_id(ui, label, response.id);
        if response.lost_focus() && text != value {
            ui.data_mut(|d| d.remove::<String>(id));
            apply(text);
        } else if response.has_focus() {
            ui.data_mut(|d| d.insert_temp(id, text));
        } else {
            ui.data_mut(|d| d.remove::<String>(id));
        }
    });
}

/// A label + read-only text (a localized node's text while a non-source locale previews).
fn locked_text_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(label);
        let mut text = value.to_owned();
        ui.add(
            egui::TextEdit::singleline(&mut text)
                .desired_width(160.0)
                .interactive(false),
        )
        .on_hover_text("Read-only while a translation previews; set Preview language to Source to edit");
    });
}

/// A failed edit stays on screen until it is dismissed (a later good edit does not hide it).
fn report<T>(error: &mut Option<String>, result: Result<T, DesignerError>) {
    if let Err(e) = result {
        *error = Some(e.to_string());
    }
}

/// [`report`], handing the value on.
fn report_ok<T>(error: &mut Option<String>, result: Result<T, DesignerError>) -> Option<T> {
    match result {
        Ok(v) => Some(v),
        Err(e) => {
            *error = Some(e.to_string());
            None
        }
    }
}

/// Document-level back handling, shown when the root is selected (ADR-8252). Numeric pivot and
/// the subtree checkbox live in [`layout_rows`]; Shift-click stays a selection gesture.
fn screen_back_rows(ui: &mut egui::Ui, doc: &mut UiDesign) {
    ui.label("Screen back");
    let current = doc.file().back;
    let mut chosen = current;
    ui.horizontal(|ui| {
        ui.radio_value(&mut chosen, None, "Stack default");
        ui.radio_value(&mut chosen, Some(ScreenBackDef::Ignore), "Ignore");
        let pop = ui.radio_value(&mut chosen, Some(ScreenBackDef::Pop), "Pop");
        #[cfg(test)]
        ui.data_mut(|d| d.insert_temp(egui::Id::new("ui-designer-back-pop"), pop.id));
        // The integration-test build drops `cfg(test)`, so the id is still read there.
        #[cfg(not(test))]
        let _ = pop.id;
    });
    let frame = ui.ctx().cumulative_frame_nr();
    let stamp = egui::Id::new("ui-designer-back-frame");
    let seen = ui.data(|d| d.get_temp::<u64>(stamp));
    if chosen != current && seen != Some(frame) {
        let mut file = doc.file().clone();
        file.back = chosen;
        let touched = doc.selection.clone().unwrap_or_default();
        doc.replace_as_step(file, touched);
    }
    if seen != Some(frame) {
        ui.data_mut(|d| d.insert_temp(stamp, frame));
    }
}

/// Edits from the layout rows: `(field key, JSON value)`, encoded with the format's own types.
fn layout_rows(ui: &mut egui::Ui, node: &UiNodeDef) -> Vec<(&'static str, Value)> {
    let style = &node.style;
    let mut out = Vec::new();
    egui::Grid::new("ui-designer-layout")
        .num_columns(2)
        .show(ui, |ui| {
            enum_row(
                ui,
                ("display", "style.display"),
                style.display,
                &[Display::Flex, Display::Grid, Display::Block, Display::None],
                &mut out,
            );
            enum_row(
                ui,
                ("position", "style.position"),
                style.position,
                &[PositionType::Relative, PositionType::Absolute],
                &mut out,
            );
            enum_row(
                ui,
                ("direction", "style.direction"),
                style.direction,
                &[Dir::Row, Dir::RowReverse, Dir::Column, Dir::ColumnReverse],
                &mut out,
            );
            enum_row(
                ui,
                ("justify", "style.justify_content"),
                style.justify_content,
                &[
                    Justify::Start,
                    Justify::End,
                    Justify::Center,
                    Justify::SpaceBetween,
                    Justify::SpaceAround,
                    Justify::SpaceEvenly,
                ],
                &mut out,
            );
            let aligns = [
                Align::Auto,
                Align::Start,
                Align::End,
                Align::Center,
                Align::Stretch,
                Align::Baseline,
            ];
            enum_row(
                ui,
                ("align items", "style.align_items"),
                style.align_items,
                &aligns,
                &mut out,
            );
            enum_row(
                ui,
                ("align self", "style.align_self"),
                style.align_self,
                &aligns,
                &mut out,
            );
            aspect_ratio_row(ui, style, &mut out);
            pair_row(ui, ("size", "style.size"), style.size, &mut out);
            pair_row(ui, ("min size", "style.min_size"), style.min_size, &mut out);
            pair_row(ui, ("max size", "style.max_size"), style.max_size, &mut out);
            pair_row(ui, ("gap", "style.gap"), style.gap, &mut out);
            edges_row(ui, ("margin", "style.margin"), style.margin, &mut out);
            edges_row(ui, ("padding", "style.padding"), style.padding, &mut out);
            edges_row(ui, ("inset", "style.inset"), style.inset, &mut out);
            number_row(ui, ("grow", "style.flex_grow"), style.flex_grow, &mut out);
            number_row(ui, ("shrink", "style.flex_shrink"), style.flex_shrink, &mut out);
            number_row(ui, ("opacity", "opacity"), node.opacity.unwrap_or(1.0), &mut out);
        });
    ui.horizontal(|ui| {
        flag(ui, ("wrap", "style.flex_wrap"), style.flex_wrap, &mut out);
        flag(ui, ("clip", "clip"), node.clip, &mut out);
        flag(ui, ("interactable", "interactable"), node.interactable, &mut out);
        flag(ui, ("disabled", "disabled"), node.disabled, &mut out);
    });
    let mut subtree = node.disabled_subtree;
    let subtree_box = ui.checkbox(&mut subtree, "Disable subtree");
    #[cfg(test)]
    ui.ctx()
        .data_mut(|d| d.insert_temp(egui::Id::new("ui-designer-subtree"), subtree_box.id));
    if subtree_box.changed() {
        out.push(("disabled_subtree", json!(subtree)));
    }
    let mut pivot = node.pivot.unwrap_or([0.5, 0.5]);
    ui.horizontal(|ui| {
        ui.label("Pivot");
        let x = crate::a11y_audit::named_drag_value(
            ui.add(egui::DragValue::new(&mut pivot[0]).speed(0.01)),
            "Pivot X",
            f64::from(pivot[0]),
        );
        #[cfg(test)]
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new("ui-designer-pivot-x"), x.id));
        let y = crate::a11y_audit::named_drag_value(
            ui.add(egui::DragValue::new(&mut pivot[1]).speed(0.01)),
            "Pivot Y",
            f64::from(pivot[1]),
        );
        #[cfg(test)]
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new("ui-designer-pivot-y"), y.id));
        if x.changed() || y.changed() {
            out.push(("pivot", json!(pivot)));
        }
    });
    text_effect_rows(ui, node, &mut out);
    // A gradient stays a JSON edit.
    let color = match &node.background {
        Some(BackgroundDef::Color(c)) => Some(c.0),
        None => Some([0.0; 4]),
        _ => None,
    };
    if let Some(mut rgba) = color {
        ui.horizontal(|ui| {
            ui.label("background");
            if ui.color_edit_button_rgba_unmultiplied(&mut rgba).changed() {
                out.push((
                    "background",
                    serde_json::to_value(BackgroundDef::Color(UiColor(rgba))).unwrap_or(Value::Null),
                ));
            }
            if node.background.is_some()
                && crate::a11y_audit::glyph_button(ui, "x", "Remove the background").clicked()
            {
                out.push(("background", Value::Null));
            }
        });
    }
    out
}

/// A label and field key.
type RowKey = (&'static str, &'static str);

fn enum_row<T: Copy + PartialEq + serde::Serialize>(
    ui: &mut egui::Ui,
    (label, key): RowKey,
    value: T,
    options: &[T],
    out: &mut Vec<(&'static str, Value)>,
) {
    let name = |v: T| match serde_json::to_value(v) {
        Ok(Value::String(s)) => s,
        _ => String::new(),
    };
    ui.label(label);
    egui::ComboBox::from_id_salt(key)
        .selected_text(name(value))
        .show_ui(ui, |ui| {
            for &option in options {
                if ui.selectable_label(option == value, name(option)).clicked() && option != value {
                    out.push((key, json!(name(option))));
                }
            }
        });
    ui.end_row();
}

/// A length's unit picker and number; returns whether it changed.
fn dimension_edit(ui: &mut egui::Ui, salt: (&'static str, usize), value: &mut Dimension) -> bool {
    let number = match *value {
        Dimension::Points(v) | Dimension::Percent(v) => v,
        Dimension::Auto => 0.0,
    };
    let unit = match value {
        Dimension::Auto => "auto",
        Dimension::Points(_) => "px",
        Dimension::Percent(_) => "%",
    };
    let mut changed = false;
    egui::ComboBox::from_id_salt(salt)
        .width(44.0)
        .selected_text(unit)
        .show_ui(ui, |ui| {
            changed |= ui.selectable_value(value, Dimension::Auto, "auto").changed();
            changed |= ui
                .selectable_value(value, Dimension::Points(number), "px")
                .changed();
            changed |= ui
                .selectable_value(value, Dimension::Percent(number), "%")
                .changed();
        });
    if let Dimension::Points(v) | Dimension::Percent(v) = value {
        changed |= ui.add(egui::DragValue::new(v).speed(1.0)).changed();
    }
    changed
}

fn pair_row(
    ui: &mut egui::Ui,
    (label, key): RowKey,
    mut value: [Dimension; 2],
    out: &mut Vec<(&'static str, Value)>,
) {
    ui.label(label);
    ui.horizontal(|ui| {
        let a = dimension_edit(ui, (key, 0), &mut value[0]);
        let b = dimension_edit(ui, (key, 1), &mut value[1]);
        if a || b {
            out.push((key, serde_json::to_value(value).unwrap_or(Value::Null)));
        }
    });
    ui.end_row();
}

fn edges_row(
    ui: &mut egui::Ui,
    (label, key): RowKey,
    mut value: Edges,
    out: &mut Vec<(&'static str, Value)>,
) {
    ui.label(label);
    ui.horizontal(|ui| {
        let mut changed = false;
        for (i, side) in [
            &mut value.left,
            &mut value.top,
            &mut value.right,
            &mut value.bottom,
        ]
        .into_iter()
        .enumerate()
        {
            changed |= dimension_edit(ui, (key, i), side);
        }
        if changed {
            out.push((key, serde_json::to_value(value).unwrap_or(Value::Null)));
        }
    });
    ui.end_row();
}

fn number_row(ui: &mut egui::Ui, (label, key): RowKey, mut value: f32, out: &mut Vec<(&'static str, Value)>) {
    ui.label(label);
    if ui.add(egui::DragValue::new(&mut value).speed(0.05)).changed() {
        out.push((key, json!(value)));
    }
    ui.end_row();
}

fn flag(ui: &mut egui::Ui, (label, key): RowKey, mut value: bool, out: &mut Vec<(&'static str, Value)>) {
    if ui.checkbox(&mut value, label).changed() {
        out.push((key, json!(value)));
    }
}

/// The project's components as chips (dragged onto a row, or clicked to add under the selection).
/// Returns the `use` path of a clicked chip. Scanned once per `assets/` directory; "Rescan" reads
/// the disk again.
fn component_palette(ui: &mut egui::Ui, doc: &UiDesign, state: &mut UiDesignerPanelState) -> Option<String> {
    let root = assets_root(doc);
    if state.components.as_ref().is_none_or(|(r, _)| *r != root) {
        state.components = Some((root.clone(), palette_components(doc)));
    }
    let mut clicked = None;
    let mut rescan = false;
    ui.horizontal_wrapped(|ui| {
        ui.label("Components");
        let found = state.components.as_ref().map_or(&[][..], |(_, c)| c.as_slice());
        if found.is_empty() {
            ui.weak(if root.is_some() {
                "none in assets/"
            } else {
                "save under assets/ first"
            });
        }
        for info in found {
            let chip = ui
                .selectable_label(false, &info.path)
                .interact(egui::Sense::click_and_drag())
                .on_hover_text(format!(
                    "Drag onto a row, or click to add under the selection. Parameters: {}",
                    info.params
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            #[cfg(test)]
            test_rect(ui, &format!("component:{}", info.path), chip.rect);
            chip.dnd_set_drag_payload(PaletteComponent(info.path.clone()));
            if chip.clicked() {
                clicked = Some(info.path.clone());
            }
        }
        rescan = ui.small_button("Rescan").clicked();
    });
    if rescan {
        state.components = None;
    }
    clicked
}

/// One hierarchy row: the node, its label and, for an instance, its inherited nodes' lines.
#[derive(Debug, Clone)]
struct TreeRow {
    path: NodePath,
    label: String,
    inherited: Vec<String>,
}

/// The hierarchy's rows, built once per document state (ADR-7326; the walk and the labels used
/// to be redone every repaint).
fn tree_rows(doc: &UiDesign, state: &mut UiDesignerPanelState) -> std::sync::Arc<[TreeRow]> {
    let key = (doc.serial(), doc.revision());
    if let Some((built, rows)) = &state.tree
        && *built == key
    {
        return std::sync::Arc::clone(rows);
    }
    let mut inherited: std::collections::HashMap<NodePath, Vec<String>> = inherited_rows(doc, state)
        .into_iter()
        .map(|(path, nodes, _)| {
            let lines = nodes
                .iter()
                .skip(1)
                .map(|(depth, label)| format!("{}{label} (inherited)", "  ".repeat(path.len() + depth)))
                .collect();
            (path, lines)
        })
        .collect();
    let mut rows: Vec<TreeRow> = Vec::new();
    walk(&doc.file().root, &mut Vec::new(), &mut |node, path| {
        let repeat = node
            .repeat
            .as_ref()
            .map(|r| format!(" [repeat {}]", r.source))
            .unwrap_or_default();
        let label = format!(
            "{}{} {}{repeat}",
            "  ".repeat(path.len()),
            node_kind(node),
            node.name.as_deref().map(|n| format!("#{n}")).unwrap_or_default()
        );
        rows.push(TreeRow {
            path: path.to_vec(),
            label,
            inherited: inherited.remove(path).unwrap_or_default(),
        });
    });
    let rows: std::sync::Arc<[TreeRow]> = rows.into();
    state.tree = Some((key, std::sync::Arc::clone(&rows)));
    rows
}

/// The inherited rows of every instance of the document, cached per revision (the panel draws
/// twice a frame; expanding reads the component files).
fn inherited_rows(doc: &UiDesign, state: &mut UiDesignerPanelState) -> Vec<InheritedRows> {
    let rev = doc.revision();
    if state.inherited.as_ref().is_none_or(|(r, _)| *r != rev) {
        let assets = assets_root(doc);
        let mut out = Vec::new();
        walk(&doc.file().root, &mut Vec::new(), &mut |n, p| {
            if n.use_component.is_none() {
                return;
            }
            let expansion = crate::ui_designer_preview::expand_instance(n, assets.as_deref());
            let mut nodes = Vec::new();
            walk(&expansion.root, &mut Vec::new(), &mut |c, cp| {
                nodes.push((cp.len(), crate::ui_designer::node_label(c)));
            });
            let problems = expansion
                .diagnostics
                .iter()
                .map(|d| format!("{} {}: {}", d.code, d.node, d.message))
                .collect();
            out.push((p.to_vec(), nodes, problems));
        });
        state.inherited = Some((rev, out));
    }
    state
        .inherited
        .as_ref()
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

/// An instance's inspector rows: its component (enter), bind root, parameters and problems.
fn instance_rows(
    ui: &mut egui::Ui,
    doc: &mut UiDesign,
    state: &mut UiDesignerPanelState,
    path: &[usize],
    node: &UiNodeDef,
    error: &mut Option<String>,
) {
    let use_path = node.use_component.clone().unwrap_or_default();
    ui.separator();
    ui.horizontal(|ui| {
        ui.label(format!("Component: {use_path}"));
        if let Some(root) = assets_root(doc)
            && ui
                .button("Enter")
                .on_hover_text("Open the component; Return comes back here")
                .clicked()
        {
            state.enter = Some(root.join(&use_path));
        }
    });
    text_row(ui, "bind_root", node.bind_root.clone().unwrap_or_default(), |v| {
        report(
            error,
            doc.apply(DesignOp::Set {
                node: path.to_vec(),
                key: "bind_root".to_owned(),
                value: if v.is_empty() { Value::Null } else { json!(v) },
            }),
        );
    });
    match load_component(doc, node) {
        Ok((_, comp)) => {
            let decl = comp.component.unwrap_or_default();
            if decl.params.is_empty() {
                ui.weak("The component declares no parameters.");
            }
            for param in &decl.params {
                let current = node.params.get(&param.name).map(param_text).unwrap_or_default();
                let label = format!(
                    "{} ({}, default {})",
                    param.name,
                    param.ty.name(),
                    param_text(&param.default)
                );
                text_row(ui, &label, current, |v| {
                    let value = param_json(param.ty, &v);
                    report(error, set_instance_param(doc, path, &param.name, value));
                });
            }
            if !decl.slots.is_empty() {
                slot_rows(ui, doc, path, node, &decl.slots, error);
            }
        }
        Err(e) => {
            ui.colored_label(egui::Color32::from_rgb(230, 110, 90), e.to_string());
        }
    }
    let problems = state
        .inherited
        .as_ref()
        .and_then(|(_, rows)| rows.iter().find(|(p, _, _)| p == path))
        .map(|(_, _, problems)| problems.clone())
        .unwrap_or_default();
    for p in problems {
        ui.colored_label(egui::Color32::from_rgb(230, 110, 90), p);
    }
}

/// One row per slot the component declares (ADR-7677, spec 111 § 2.2): what fills it, a picker
/// that sets its content to one palette node, a drop target that appends a dragged palette chip,
/// and Clear (the component's default content shows again). Each is one undo step through
/// `set_instance_slot`, the function `editor.ui_designer_set_instance_slot` calls.
fn slot_rows(
    ui: &mut egui::Ui,
    doc: &mut UiDesign,
    path: &[usize],
    node: &UiNodeDef,
    slots: &[String],
    error: &mut Option<String>,
) {
    ui.label("Slots");
    for slot in slots {
        let stored = node.slots.get(slot);
        let content = stored.cloned().unwrap_or_default();
        let summary = slot_summary(stored.map(Vec::as_slice));
        let mut set = None;
        ui.horizontal(|ui| {
            let target = ui
                .selectable_label(false, format!("{slot}: {summary}"))
                .interact(egui::Sense::click_and_drag())
                .on_hover_text("Drop a palette chip here to add it to the slot");
            #[cfg(test)]
            test_rect(ui, &format!("slot-row:{slot}"), target.rect);
            if target.dnd_hover_payload::<PaletteNode>().is_some() {
                drop_marker(ui, target.rect, DropZone::Into);
            }
            if let Some(chip) = release_payload::<PaletteNode>(&target)
                && let Some(added) = preset(chip.0)
            {
                let mut next = content.clone();
                next.push(added);
                set = Some(Some(next));
            }
            let combo = egui::ComboBox::from_id_salt(("ui-designer-slot", slot.as_str()))
                .selected_text("Set…")
                .show_ui(ui, |ui| {
                    for name in PRESETS {
                        let item = ui.selectable_label(false, name);
                        #[cfg(test)]
                        test_rect(ui, &format!("slot:{slot}:{name}"), item.rect);
                        if item.clicked() {
                            set = preset(name).map(|n| Some(vec![n]));
                        }
                    }
                });
            #[cfg(test)]
            test_rect(ui, &format!("slot:{slot}"), combo.response.rect);
            let _ = combo;
            // An instance that stores an empty list fills the slot with nothing, which hides the
            // component's default: Clear removes the entry like it removes any other content.
            let clear = ui
                .add_enabled(stored.is_some(), egui::Button::new("Clear"))
                .on_hover_text("Show the component's default content again");
            #[cfg(test)]
            test_rect(ui, &format!("slot-clear:{slot}"), clear.rect);
            if clear.clicked() {
                set = Some(None);
            }
        });
        if let Some(content) = set {
            report(error, set_instance_slot(doc, path, slot, content));
        }
    }
}

/// What the slot rows say fills a slot: no entry is the component's default content, an entry
/// of no nodes is an explicitly empty slot (the default is hidden, ADR-7716), otherwise the
/// nodes' names.
fn slot_summary(stored: Option<&[UiNodeDef]>) -> String {
    match stored {
        None => "component default".to_owned(),
        Some([]) => "empty (hides the default)".to_owned(),
        Some(nodes) => nodes
            .iter()
            .map(|n| n.name.clone().unwrap_or_else(|| node_kind(n).to_owned()))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// The themed preview's theme picker (ADR-7677): the active theme or one of the editor's and the
/// project's themes, stored on the document's preview settings like the agent's `theme`.
fn theme_picker(ui: &mut egui::Ui, doc: &mut UiDesign, state: &UiDesignerPanelState) {
    let names = state
        .theme_names
        .as_ref()
        .map(|(_, names)| names.as_slice())
        .unwrap_or_default();
    let current = doc.preview.theme.clone();
    let combo = egui::ComboBox::from_id_salt("ui-designer-theme")
        .selected_text(current.as_deref().unwrap_or("active theme"))
        .show_ui(ui, |ui| {
            let item = ui.selectable_label(current.is_none(), "active theme");
            #[cfg(test)]
            test_rect(ui, "theme:active", item.rect);
            if item.clicked() {
                doc.preview.theme = None;
            }
            for name in names {
                let item = ui.selectable_label(current.as_deref() == Some(name), name);
                #[cfg(test)]
                test_rect(ui, &format!("theme:{name}"), item.rect);
                if item.clicked() {
                    doc.preview.theme = Some(name.clone());
                }
            }
        });
    #[cfg(test)]
    test_rect(ui, "theme", combo.response.rect);
    let _ = combo;
}

/// The preview's custom window size and safe-area insets (ADR-7677), view state of the open
/// document: never written to the file and not an undo step. The insets go through the runtime's
/// `SafeArea` and are limited to the window as the runtime limits them; the agent's `size` and
/// `safe_area` set the same settings.
fn size_rows(ui: &mut egui::Ui, doc: &mut UiDesign, error: &mut Option<String>) {
    let mut size = [
        doc.preview.size[0].round().max(0.0) as u32,
        doc.preview.size[1].round().max(0.0) as u32,
    ];
    let mut safe = doc.preview.safe_area_insets();
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.label("size");
        changed |= number_field(ui, "preview width", &mut size[0]);
        ui.label("x");
        changed |= number_field(ui, "preview height", &mut size[1]);
        ui.label("safe area");
        changed |= number_field(ui, "inset left", &mut safe.left);
        changed |= number_field(ui, "inset top", &mut safe.top);
        changed |= number_field(ui, "inset right", &mut safe.right);
        changed |= number_field(ui, "inset bottom", &mut safe.bottom);
    });
    if changed {
        report(error, doc.preview.set_window(size, safe));
    }
}

/// A small whole-number field (hover text `key`) that writes `value` when editing ends with a
/// number; other text is dropped.
fn number_field(ui: &mut egui::Ui, key: &str, value: &mut u32) -> bool {
    let id = ui.id().with(("ui-designer-number", key));
    let mut text = ui
        .data_mut(|d| d.get_temp::<String>(id))
        .unwrap_or_else(|| value.to_string());
    let response = ui
        .add(egui::TextEdit::singleline(&mut text).desired_width(44.0))
        .on_hover_text(key);
    #[cfg(test)]
    test_rect(ui, key, response.rect);
    if response.lost_focus() {
        ui.data_mut(|d| d.remove::<String>(id));
        if let Ok(n) = text.trim().parse::<u32>()
            && n != *value
        {
            *value = n;
            return true;
        }
    } else if response.has_focus() {
        ui.data_mut(|d| d.insert_temp(id, text));
    } else {
        ui.data_mut(|d| d.remove::<String>(id));
    }
    false
}

/// A parameter value as the inspector's text field shows it.
fn param_text(value: &regia_ui::ui_component::ParamValue) -> String {
    use regia_ui::ui_component::ParamValue as V;
    match value {
        V::Bool(b) => b.to_string(),
        V::Number(n) => n.to_string(),
        V::Text(t) => t.clone(),
        V::Channels(c) => format!("{c:?}"),
    }
}

/// The JSON value a parameter's text field means: empty resets (null); numbers and flags parse, a
/// colour is a hex string or `[r, g, b, a]`.
fn param_json(ty: regia_ui::ui_component::ParamType, text: &str) -> Value {
    use regia_ui::ui_component::ParamType as T;
    let text = text.trim();
    if text.is_empty() {
        return Value::Null;
    }
    match ty {
        T::Number => text.parse::<f64>().map_or_else(|_| json!(text), |n| json!(n)),
        T::Bool => match text {
            "true" => json!(true),
            "false" => json!(false),
            _ => json!(text),
        },
        T::Color if text.starts_with('[') => serde_json::from_str(text).unwrap_or_else(|_| json!(text)),
        T::String | T::Color => json!(text),
    }
}

/// "Make component" for the selected node: a path under `assets/`, the node's properties to expose
/// as parameters, and the button (one undo step; the component file is written first).
fn make_component_rows(
    ui: &mut egui::Ui,
    doc: &mut UiDesign,
    state: &mut UiDesignerPanelState,
    path: &[usize],
    node: &UiNodeDef,
    error: &mut Option<String>,
) {
    use regia_ui::ui_component::ParamProperty;
    ui.collapsing("Make component", |ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.make_path)
                .hint_text("ui/health_bar.ui.ron")
                .desired_width(200.0),
        );
        ui.horizontal_wrapped(|ui| {
            for property in ParamProperty::ALL {
                let fits = match property {
                    ParamProperty::Tint => node.image.is_some(),
                    ParamProperty::Progress => {
                        matches!(node.widget, Some(regia_ui::authoring::WidgetDef::Progress(_)))
                    }
                    _ => true,
                };
                if !fits {
                    continue;
                }
                let mut on = state.make_expose.contains(&property);
                if ui.checkbox(&mut on, property.name()).changed() {
                    if on {
                        state.make_expose.push(property);
                    } else {
                        state.make_expose.retain(|p| *p != property);
                    }
                }
            }
        });
        if ui
            .add_enabled(
                !state.make_path.trim().is_empty(),
                egui::Button::new("Make component"),
            )
            .on_hover_text("Write the node to a new component and replace it with an instance")
            .clicked()
        {
            let exposed: Vec<Exposed> = state
                .make_expose
                .iter()
                .map(|p| Exposed {
                    name: p.name().to_owned(),
                    node: path.to_vec(),
                    property: *p,
                })
                .collect();
            match make_component(doc, path, state.make_path.trim(), &exposed) {
                Ok(_) => {
                    state.make_expose.clear();
                    state.components = None;
                }
                Err(e) => *error = Some(e.to_string()),
            }
        }
    });
}

/// Records a widget's rect so tests can click it (egui has no widget lookup).
#[cfg(test)]
fn test_rect(ui: &egui::Ui, key: &str, rect: egui::Rect) {
    ui.data_mut(|d| d.insert_temp(egui::Id::new(("ui-designer-test-rect", key)), rect));
}

/// Presets are buttons (Tab then Space/Enter), arranged as three fixed axes plus stretch.
fn anchor_rows(
    ui: &mut egui::Ui,
    doc: &mut UiDesign,
    state: &mut UiDesignerPanelState,
    path: &[usize],
    error: &mut Option<String>,
) {
    if path.is_empty() {
        return;
    }
    egui::CollapsingHeader::new("Anchor presets")
        .default_open(true)
        .show(ui, |ui| {
            let mut keep = !state.snap_anchor_position;
            ui.checkbox(&mut keep, "Keep position");
            if state.fresh_input {
                state.snap_anchor_position = !keep;
            }
            if !keep {
                ui.weak("Snap to preset");
            }
            use regia_ui::anchors::AnchorPreset::*;
            let cells = [
                (TopLeft, "Top left"),
                (TopCenter, "Top"),
                (TopRight, "Top right"),
                (TopWide, "Top stretch"),
                (CenterLeft, "Left"),
                (Center, "Center"),
                (CenterRight, "Right"),
                (VCenterWide, "Width stretch"),
                (BottomLeft, "Bottom left"),
                (BottomCenter, "Bottom"),
                (BottomRight, "Bottom right"),
                (BottomWide, "Bottom stretch"),
                (LeftWide, "Left stretch"),
                (HCenterWide, "Height stretch"),
                (RightWide, "Right stretch"),
                (FullRect, "Full stretch"),
            ];
            let cell_width = ((ui.available_width() - 3.0 * ui.spacing().item_spacing.x) / 4.0).max(1.0);
            egui::Grid::new("ui-anchor-presets")
                .min_col_width(0.0)
                .num_columns(4)
                .show(ui, |ui| {
                    for (i, (preset, label)) in cells.into_iter().enumerate() {
                        let response = ui.add_sized(
                            [cell_width, ui.spacing().interact_size.y * 2.0],
                            egui::Button::new(label).wrap(),
                        );
                        #[cfg(test)]
                        if preset == TopRight {
                            ui.ctx().data_mut(|d| {
                                d.insert_temp(egui::Id::new("ui-anchor-top-right"), response.id)
                            });
                        }
                        if response.clicked() && state.fresh_input {
                            let localization = doc
                                .preview
                                .assets_root
                                .as_deref()
                                .and_then(Path::parent)
                                .and_then(project_localization);
                            let edit = preview(doc.file(), &doc.preview, localization).and_then(|frame| {
                                crate::ui_designer_canvas::anchor_preset_edit(
                                    doc.file(),
                                    &frame,
                                    path,
                                    preset,
                                    keep,
                                    |file| {
                                        let localization = doc
                                            .preview
                                            .assets_root
                                            .as_deref()
                                            .and_then(Path::parent)
                                            .and_then(project_localization);
                                        preview(file, &doc.preview, localization)
                                    },
                                )
                            });
                            match edit {
                                Ok(op) => report(error, doc.apply(op)),
                                Err(e) => *error = Some(e.to_string()),
                            }
                        }
                        if (i + 1).is_multiple_of(4) {
                            ui.end_row();
                        }
                    }
                });
        });
}

fn aspect_ratio_row(ui: &mut egui::Ui, style: &regia_ui::Style, out: &mut Vec<(&'static str, Value)>) {
    let fresh = fresh_row_input(ui, "ui-aspect-row-frame");
    ui.label("Aspect ratio");
    ui.horizontal(|ui| {
        let mut ratio = style.aspect_ratio.unwrap_or(1.0);
        let locked = style.aspect_ratio.is_some();
        let response = crate::a11y_audit::glyph_button(
            ui,
            if locked { "🔒" } else { "🔓" },
            if locked {
                "Unlock aspect ratio"
            } else {
                "Lock aspect ratio"
            },
        );
        #[cfg(test)]
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new("ui-aspect-lock"), response.id));
        if response.clicked() && fresh {
            out.push((
                "style.aspect_ratio",
                if locked { Value::Null } else { json!(ratio) },
            ));
        }
        let response = ui.add_enabled(
            locked,
            egui::DragValue::new(&mut ratio).speed(0.01).range(0.001..=1000.0),
        );
        if crate::a11y_audit::named_drag_value(response, "Aspect ratio", f64::from(ratio)).changed() && fresh
        {
            out.push(("style.aspect_ratio", json!(ratio)));
        }
    });
    ui.end_row();
}

fn text_effect_rows(ui: &mut egui::Ui, node: &UiNodeDef, out: &mut Vec<(&'static str, Value)>) {
    let _span = tracing::trace_span!("ui_designer_effect_rows").entered();
    let Some(text) = &node.text else {
        return;
    };
    let fresh = fresh_row_input(ui, "ui-effects-row-frame");
    let mut layers = text.style.effects.clone();
    let mut remove = None;
    let mut swap = None;
    ui.label("Text effects (front to back)");
    let len = layers.len();
    for (index, layer) in layers.iter_mut().enumerate() {
        ui.push_id(("ui-text-effect", index), |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("Layer {}", index + 1));
                let response = ui.color_edit_button_rgba_unmultiplied(&mut layer.color.0);
                crate::a11y_audit::name(response, egui::WidgetType::Button, "Effect color");
                if ui.add_enabled(index > 0, egui::Button::new("Up")).clicked() {
                    swap = Some((index, index - 1));
                }
                if ui
                    .add_enabled(index + 1 < len, egui::Button::new("Down"))
                    .clicked()
                {
                    swap = Some((index, index + 1));
                }
                if ui.button("Remove layer").clicked() {
                    remove = Some(index);
                }
            });
            ui.horizontal_wrapped(|ui| {
                for (label, value, positive) in [
                    ("Width", &mut layer.width, true),
                    ("Offset X", &mut layer.offset[0], false),
                ] {
                    ui.label(label);
                    let mut field = egui::DragValue::new(&mut *value).speed(0.1);
                    if positive {
                        field = field.range(0.0..=1024.0);
                    }
                    let response = ui.add(field);
                    crate::a11y_audit::named_drag_value(response, label, f64::from(*value));
                }
                ui.label("Offset Y");
                let response = ui.add(egui::DragValue::new(&mut layer.offset[1]).speed(0.1));
                crate::a11y_audit::named_drag_value(response, "Offset Y", f64::from(layer.offset[1]));
                ui.label("Blur");
                let response = ui.add(
                    egui::DragValue::new(&mut layer.blur)
                        .speed(0.1)
                        .range(0.0..=1024.0),
                );
                crate::a11y_audit::named_drag_value(response, "Blur", f64::from(layer.blur));
            });
        });
    }
    if let Some(index) = remove {
        layers.remove(index);
    } else if let Some((a, b)) = swap {
        layers.swap(a, b);
    }
    ui.horizontal(|ui| {
        let enabled = layers.len() < regia_ui::text_effects::MAX_TEXT_EFFECTS;
        let add = ui.add_enabled(enabled, egui::Button::new("Add outline"));
        #[cfg(test)]
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new("ui-effect-add-outline"), add.id));
        let tokens = crate::theme::ui_tokens(ui.ctx());
        if add.clicked() {
            layers.push(regia_ui::text_effects::TextEffect::outline(
                1.0,
                UiColor(egui::Rgba::from(tokens.text).to_array()),
            ));
        }
        if ui.add_enabled(enabled, egui::Button::new("Add shadow")).clicked() {
            layers.push(regia_ui::text_effects::TextEffect::shadow(
                [2.0, 2.0],
                2.0,
                UiColor(egui::Rgba::from(tokens.shade).to_array()),
            ));
        }
    });
    if layers != text.style.effects && fresh {
        out.push((
            "text.style.effects",
            serde_json::to_value(layers).unwrap_or(Value::Null),
        ));
    }
}

fn preview_scale_rows(ui: &mut egui::Ui, settings: &mut PreviewSettings) {
    let fresh = fresh_row_input(ui, "ui-preview-scale-frame");
    let before = (settings.scale, settings.scale_mode, settings.dpi);
    ui.horizontal_wrapped(|ui| {
        let mut manual = settings.scale.is_some();
        let response = ui.checkbox(&mut manual, "Manual scale");
        #[cfg(test)]
        ui.ctx()
            .data_mut(|d| d.insert_temp(egui::Id::new("ui-manual-scale"), response.id));
        if response.changed() {
            settings.scale = manual.then_some(1.0);
        }
        if let Some(scale) = &mut settings.scale {
            let response = ui.add(
                egui::DragValue::new(&mut *scale)
                    .range(0.05..=16.0)
                    .speed(0.05)
                    .suffix("x"),
            );
            crate::a11y_audit::named_drag_value(response, "Preview scale", f64::from(*scale));
        } else {
            use regia_ui::ScaleMode;
            let modes = [
                ("Runtime default", None),
                ("Pixels", Some(ScaleMode::ConstantPixelSize)),
                ("Display DPI", Some(ScaleMode::ConstantPhysicalSize)),
                ("Reference size", Some(ScaleMode::default())),
                (
                    "Integer",
                    Some(ScaleMode::IntegerScale {
                        reference: [1920.0, 1080.0],
                    }),
                ),
            ];
            let label = modes
                .iter()
                .find(|(_, m)| *m == settings.scale_mode)
                .map_or("Custom rule", |(label, _)| *label);
            egui::ComboBox::from_id_salt("ui-preview-scale-rule")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    for (label, mode) in modes {
                        ui.selectable_value(&mut settings.scale_mode, mode, label);
                    }
                });
            if let Some(
                ScaleMode::ScaleWithScreen { reference, .. } | ScaleMode::IntegerScale { reference },
            ) = &mut settings.scale_mode
            {
                for (index, value) in reference.iter_mut().enumerate() {
                    let response = ui.add(egui::DragValue::new(&mut *value).range(1.0..=8192.0));
                    crate::a11y_audit::named_drag_value(
                        response,
                        if index == 0 {
                            "Reference width"
                        } else {
                            "Reference height"
                        },
                        f64::from(*value),
                    );
                }
            }
            if let Some(ScaleMode::ScaleWithScreen { match_width, .. }) = &mut settings.scale_mode {
                ui.add(egui::Slider::new(match_width, 0.0..=1.0).text("Match width"));
            }
            ui.label("DPI");
            let response = ui.add(egui::DragValue::new(&mut settings.dpi).range(24.0..=1536.0));
            crate::a11y_audit::named_drag_value(response, "Preview display DPI", f64::from(settings.dpi));
        }
    });
    if !fresh {
        (settings.scale, settings.scale_mode, settings.dpi) = before;
    }
}

/// A row may be drawn again in an egui discard pass; commit its edit only once.
fn fresh_row_input(ui: &egui::Ui, salt: &str) -> bool {
    let id = ui.id().with(salt);
    let frame = ui.ctx().cumulative_frame_nr();
    ui.ctx().data_mut(|data| {
        let fresh = data.get_temp::<u64>(id) != Some(frame);
        data.insert_temp(id, frame);
        fresh
    })
}

fn interactive_canvas(
    ui: &mut egui::Ui,
    doc: &UiDesign,
    state: &mut UiDesignerPanelState,
    runtime: &mut crate::ui_designer_interactive::DesignerRuntime,
    project: Option<&std::path::Path>,
    error: &mut Option<String>,
) {
    if let Err(e) = runtime.ensure(doc, &state.runtime_choice, || {
        project.and_then(project_localization)
    }) {
        *error = Some(e.to_string());
        return;
    }
    let Some(session) = runtime.session.as_mut() else {
        return;
    };
    let commands = session.commands();
    let history = egui::CollapsingHeader::new(format!("Commands ({})", commands.len()))
        .id_salt("designer-interactive-commands")
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(ui.spacing().interact_size.y * 6.0)
                .show_rows(ui, ui.spacing().interact_size.y, commands.len(), |ui, range| {
                    for command in &commands[range] {
                        ui.label(format!(
                            "Frame {} · {}",
                            command["frame"],
                            command["value"].as_str().unwrap_or_default()
                        ));
                    }
                });
        });
    #[cfg(test)]
    ui.data_mut(|d| {
        d.insert_temp(egui::Id::new("preview-commands-id"), history.header_response.id);
        d.insert_temp(
            egui::Id::new("preview-commands-open"),
            history.body_response.is_some(),
        );
    });
    #[cfg(not(test))]
    let _ = history;
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let available = [ui.available_width().max(64.0), ui.available_height().max(120.0)];
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(available[0], available[1]),
        egui::Sense::click_and_drag(),
    );
    crate::canvas_keys::name_canvas(
        ui.ctx(),
        response.id,
        "Interactive UI preview. Tab moves between sample widgets; Escape stops playback.",
    );
    #[cfg(test)]
    {
        test_rect(ui, "interactive:canvas", rect);
        ui.data_mut(|d| d.insert_temp(egui::Id::new("interactive-canvas-id"), response.id));
    }
    if response.clicked() {
        response.request_focus();
    }
    if response.has_focus() {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    ..Default::default()
                },
            )
        });
    }
    let scale = state.view.scale(available, doc.preview.size);
    let offset = state.view.origin(available, doc.preview.size);
    let origin = rect.min + egui::vec2(offset[0], offset[1]);
    if state.fresh_input {
        let events = ui.input(|i| i.events.clone());
        for event in &events {
            let accept = match event {
                egui::Event::PointerMoved(_) | egui::Event::PointerGone => true,
                egui::Event::PointerButton { pos, pressed, .. } => {
                    rect.contains(*pos) || (!pressed && response.has_focus())
                }
                egui::Event::MouseWheel { .. } => response.hovered(),
                _ => response.has_focus(),
            };
            if accept {
                if matches!(event,egui::Event::PointerMoved(pos) if !rect.contains(*pos)) {
                    session.egui_event(&egui::Event::PointerGone, origin, scale);
                } else {
                    session.egui_event(event, origin, scale);
                }
            }
        }
        session.step();
    }
    let frame = session.frame();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, tokens.well);
    let screen = egui::Rect::from_min_size(origin, egui::vec2(frame.size[0] * scale, frame.size[1] * scale));
    if state.fresh_input
        && let Some((device, _)) = &state.themed_inputs
    {
        match session.render(device) {
            Ok(frame) => {
                let image = frame.to_color_image();
                if let Some(texture) = &mut state.live_texture {
                    texture.set(image, egui::TextureOptions::LINEAR);
                } else {
                    state.live_texture = Some(ui.ctx().load_texture(
                        "ui-designer-live",
                        image,
                        egui::TextureOptions::LINEAR,
                    ));
                }
            }
            Err(e) => *error = Some(e.to_string()),
        }
    }
    if let Some(texture) = &state.live_texture {
        painter.image(
            texture.id(),
            screen,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    } else {
        for node in frame.nodes.iter().filter(|n| n.visible && n.path != "/") {
            let r = egui::Rect::from_min_size(
                origin + egui::vec2(node.rect[0] * scale, node.rect[1] * scale),
                egui::vec2(node.rect[2] * scale, node.rect[3] * scale),
            );
            let clip = egui::Rect::from_min_size(
                origin + egui::vec2(node.clip[0] * scale, node.clip[1] * scale),
                egui::vec2(node.clip[2] * scale, node.clip[3] * scale),
            )
            .intersect(rect);
            let painter = painter.with_clip_rect(clip);
            painter.rect_filled(r, 0.0, tokens.raised);
            painter.rect_stroke(
                r,
                0.0,
                egui::Stroke::new(1.0, tokens.line),
                egui::StrokeKind::Inside,
            );
            if let Some(text) = &node.text {
                painter.text(
                    r.center(),
                    egui::Align2::CENTER_CENTER,
                    text,
                    egui::TextStyle::Body.resolve(ui.style()),
                    tokens.text,
                );
            }
        }
    }
    if response.has_focus() {
        painter.rect_stroke(
            rect,
            0.0,
            egui::Stroke::new(1.0, tokens.signal),
            egui::StrokeKind::Inside,
        );
    }
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(16));
}

fn style_section(
    ui: &mut egui::Ui,
    doc: &UiDesign,
    state: &mut UiDesignerPanelState,
    runtime: &mut crate::ui_designer_interactive::DesignerRuntime,
    project: Option<&std::path::Path>,
    error: &mut Option<String>,
) {
    let header = egui::CollapsingHeader::new("Style")
        .id_salt("designer-style-sources")
        .show(ui, |ui| {
            let path = path_string(&doc.selection.clone().unwrap_or_default());
            if let Err(e) = runtime.ensure(doc, &state.runtime_choice, || {
                project.and_then(project_localization)
            }) {
                *error = Some(e.to_string());
                return;
            }
            let Some(session) = runtime.session.as_mut() else {
                return;
            };
            let stamp = (
                doc.serial(),
                doc.revision(),
                runtime.generation,
                session.frame_number(),
                path.clone(),
            );
            if state.style_sources.as_ref().is_none_or(|(old, _)| *old != stamp) {
                match session.inspect(&path) {
                    Ok(inspection) => state.style_sources = Some((stamp, inspection)),
                    Err(e) => {
                        *error = Some(e.to_string());
                        return;
                    }
                }
            }
            let Some((_, inspection)) = &state.style_sources else {
                return;
            };
            let theme = inspection["theme"]["theme"].as_str().unwrap_or_default();
            // The theme's style rules (`.theme.ron` v2) that match the node, by selector text in
            // apply order: a later rule wins a property both set.
            let rules: Vec<&str> = inspection["theme"]["rules"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            #[cfg(test)]
            ui.data_mut(|d| {
                let shown: Vec<String> = rules.iter().map(|r| format!("rule {r}")).collect();
                d.insert_temp(egui::Id::new("style-rules-shown"), shown);
            });
            if !rules.is_empty() {
                ui.strong("Matching rules");
                for rule in rules {
                    ui.label(format!("rule {rule}"));
                }
            }
            let properties = inspection["properties"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            for property in properties {
                ui.strong(format!(
                    "{} ({})",
                    property["name"].as_str().unwrap_or_default(),
                    property["group"].as_str().unwrap_or_default()
                ));
                for source in property["sources"].as_array().into_iter().flatten() {
                    let winning = source["winning"] == true;
                    let text = format!(
                        "{}: {} — {}",
                        source["kind"].as_str().unwrap_or("default"),
                        source["detail"].as_str().unwrap_or_default(),
                        source["value"]
                    );
                    let text = egui::RichText::new(text);
                    ui.label(if winning { text } else { text.strikethrough() });
                    for token in source["tokens"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        let jump = ui.button(format!("Open token {token}"));
                        #[cfg(test)]
                        ui.data_mut(|d| d.insert_temp(egui::Id::new(("style-token-id", token)), jump.id));
                        if jump.clicked() && state.fresh_input {
                            state.theme_jump = Some((theme.to_owned(), token.to_owned()));
                        }
                    }
                }
            }
        });
    #[cfg(test)]
    ui.data_mut(|d| d.insert_temp(egui::Id::new("style-section-id"), header.header_response.id));
    #[cfg(not(test))]
    let _ = header;
}

/// Records a widget's id so keyboard tests can focus it.
#[cfg(test)]
fn test_id(ui: &egui::Ui, key: &str, id: egui::Id) {
    ui.data_mut(|d| d.insert_temp(egui::Id::new(("ui-designer-test-id", key)), id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ui::{UiMigrators, parse_ui_ron, write_ui_ron};

    const WINDOW: egui::Vec2 = egui::vec2(1600.0, 1000.0);

    /// A headless panel with egui input events.
    struct Panel {
        ctx: egui::Context,
        designer: UiDesigner,
        state: UiDesignerPanelState,
        runtime: crate::ui_designer_interactive::DesignerRuntime,
    }

    impl Panel {
        /// An open document with one slider bound to `Model.field`, selected.
        fn new() -> Self {
            let mut designer = UiDesigner::default();
            designer.new_document();
            let doc = designer.doc_mut().expect("doc");
            for _ in 0..3 {
                doc.apply(DesignOp::Add {
                    parent: Vec::new(),
                    index: None,
                    node: Box::new(preset("slider").expect("preset")),
                })
                .expect("add");
            }
            doc.apply(DesignOp::Set {
                node: vec![0],
                key: "bindings".to_owned(),
                value: json!([{ "source": "Model.field", "target": "value" }]),
            })
            .expect("bind");
            let mut panel = Self {
                ctx: egui::Context::default(),
                designer,
                state: UiDesignerPanelState::default(),
                runtime: Default::default(),
            };
            panel.frame(Vec::new());
            panel
        }

        /// An open document from `.ui.ron` text, root selected.
        fn with_text(text: &str) -> Self {
            let mut designer = UiDesigner::default();
            designer.open_text(text, None).expect("open");
            let mut panel = Self {
                ctx: egui::Context::default(),
                designer,
                state: UiDesignerPanelState::default(),
                runtime: Default::default(),
            };
            panel.frame(Vec::new());
            panel
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            self.frame_with(events, egui::Modifiers::NONE);
        }

        fn frame_with(&mut self, events: Vec<egui::Event>, modifiers: egui::Modifiers) {
            let mut events = events;
            events.insert(0, egui::Event::ModifiersChanged(modifiers));
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, WINDOW)),
                events,
                ..Default::default()
            };
            let (designer, state, runtime) = (&mut self.designer, &mut self.state, &mut self.runtime);
            let mut output = self
                .ctx
                .run_ui(raw, |ui| draw(ui, designer, state, runtime, None, None));
            // No renderer here: the font atlas upload is dropped on purpose.
            output.textures_delta.clear();
        }

        fn rect(&self, key: &str) -> egui::Rect {
            let id = egui::Id::new(("ui-designer-test-rect", key));
            self.ctx
                .data_mut(|d| d.get_temp::<egui::Rect>(id))
                .unwrap_or_else(|| panic!("no rect for {key}"))
        }

        fn doc(&mut self) -> &mut UiDesign {
            self.designer.doc_mut().expect("doc")
        }

        fn hover(&mut self, pos: egui::Pos2) {
            self.frame(vec![egui::Event::PointerMoved(pos)]);
        }

        fn button(&mut self, pos: egui::Pos2, pressed: bool) {
            self.frame(vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            }]);
        }

        fn click(&mut self, pos: egui::Pos2) {
            self.hover(pos);
            self.button(pos, true);
            self.button(pos, false);
            self.frame(Vec::new());
        }

        fn key(&mut self, key: egui::Key, modifiers: egui::Modifiers) {
            self.frame(vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }]);
        }

        /// Scroll the actual inspector before interacting with a field below the viewport.
        fn reveal(&mut self, row: &str) {
            for _ in 0..8 {
                let rect = self.rect(row);
                // Preview toolbar fields are outside the inspector's scroll area.
                if rect.center().x < WINDOW.x * (2.0 / 3.0) {
                    return;
                }
                if rect.top() >= 150.0 && rect.bottom() <= WINDOW.y - 40.0 {
                    return;
                }
                self.hover(egui::pos2(rect.center().x, WINDOW.y * 0.5));
                self.frame(vec![egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, WINDOW.y * 0.5 - rect.center().y),
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::NONE,
                }]);
                for _ in 0..20 {
                    self.frame(Vec::new());
                }
            }
            panic!(
                "inspector control {row} could not be scrolled into view: {:?}",
                self.rect(row)
            );
        }

        /// Clicks the text field `row`, replaces its text and presses Enter.
        fn type_into(&mut self, row: &str, text: &str) {
            self.reveal(row);
            self.click(self.rect(row).center());
            self.key(egui::Key::A, egui::Modifiers::COMMAND);
            self.frame(vec![egui::Event::Text(text.to_owned())]);
            self.key(egui::Key::Enter, egui::Modifiers::NONE);
            self.frame(Vec::new());
            self.frame(Vec::new());
        }
    }

    #[test]
    fn ui_designer_play_and_escape_are_keyboard_operable_without_document_edits() {
        let mut panel = Panel::new();
        panel.ctx.all_styles_mut(|style| style.animation_time = 0.0);
        let before = panel.doc().text().expect("text");
        let play = panel
            .ctx
            .data_mut(|d| d.get_temp::<egui::Id>(egui::Id::new("preview-play-id")))
            .expect("Play id");
        panel.ctx.memory_mut(|m| m.request_focus(play));
        panel.key(egui::Key::Space, egui::Modifiers::NONE);
        assert!(panel.runtime.interactive);
        panel.frame(Vec::new());
        let commands = panel
            .ctx
            .data_mut(|d| d.get_temp::<egui::Id>(egui::Id::new("preview-commands-id")))
            .expect("commands id");
        panel.ctx.memory_mut(|m| m.request_focus(commands));
        panel.key(egui::Key::Enter, egui::Modifiers::NONE);
        panel.frame(Vec::new());
        assert!(
            panel
                .ctx
                .data_mut(|d| d.get_temp::<bool>(egui::Id::new("preview-commands-open")))
                .unwrap_or(false)
        );
        let canvas = panel
            .ctx
            .data_mut(|d| d.get_temp::<egui::Id>(egui::Id::new("interactive-canvas-id")))
            .expect("canvas id");
        panel.ctx.memory_mut(|m| m.request_focus(canvas));
        panel.key(egui::Key::Tab, egui::Modifiers::NONE);
        assert!(panel.runtime.interactive);
        panel.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!panel.runtime.interactive);
        assert_eq!(panel.doc().text().expect("text"), before);
    }

    #[test]
    fn ui_designer_interactive_steps_once_when_egui_repeats_a_pass() {
        let mut panel = Panel::new();
        panel.runtime.interactive = true;
        panel.frame(Vec::new());
        let before = panel.runtime.session.as_ref().expect("session").frame_number();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, WINDOW)),
            ..Default::default()
        };
        let mut passes = 0;
        let _ = panel.ctx.run_ui(raw, |ui| {
            draw(
                ui,
                &mut panel.designer,
                &mut panel.state,
                &mut panel.runtime,
                None,
                None,
            );
            passes += 1;
            if passes == 1 {
                ui.ctx()
                    .request_discard("test: repeat the interactive canvas pass");
            }
        });
        assert_eq!(passes, 2);
        assert_eq!(
            panel.runtime.session.as_ref().expect("session").frame_number(),
            before + 1
        );
    }

    #[test]
    fn ui_designer_style_section_and_token_link_are_keyboard_operable() {
        let mut panel = Panel::new();
        panel.ctx.all_styles_mut(|style| style.animation_time = 0.0);
        let section = panel
            .ctx
            .data_mut(|d| d.get_temp::<egui::Id>(egui::Id::new("style-section-id")))
            .expect("Style id");
        panel.ctx.memory_mut(|m| m.request_focus(section));
        panel.key(egui::Key::Space, egui::Modifiers::NONE);
        panel.frame(Vec::new());
        panel.frame(Vec::new());
        assert!(panel.state.style_sources.is_some(), "Style opens with keyboard");
        let token = panel
            .ctx
            .data_mut(|d| d.get_temp::<egui::Id>(egui::Id::new(("style-token-id", "radius.md"))))
            .expect("token id");
        panel.ctx.memory_mut(|m| m.request_focus(token));
        panel.key(egui::Key::Enter, egui::Modifiers::NONE);
        assert_eq!(
            panel.state.theme_jump.as_ref().map(|(_, token)| token.as_str()),
            Some("radius.md")
        );
    }

    /// The Style section lists the `.theme.ron` v2 rules that match the selected node by their
    /// selector text (ADR-8105 (e)), reached from the keyboard like the rest of the section.
    #[test]
    fn ui_designer_style_section_shows_matching_rule_selectors() {
        let mut panel = Panel::with_text(
            r#"(version: 11, root: (children: [(name: "go", widget: button, classes: ["hot"], style: (size: (96, 32)))]))"#,
        );
        let mut themes = regia_ui::UiThemes::default();
        let name = themes
            .set
            .load_ron(
                r##"(format_version: 2, name: "rules", extends: Some("regia-dark"), rules: [
                    (widget: Some("Button"), props: { "radius": 2 }),
                    (classes: ["hot"], state: Some(hover), props: { "background": "#ff0000" }),
                ])"##,
                "rules.theme.ron",
            )
            .expect("theme");
        panel.state.runtime_choice.themes = Some(themes);
        panel.state.runtime_choice.active = Some(regia_ui::ActiveTheme::named(&name));
        panel.doc().select(Some(vec![0])).expect("select");
        panel.ctx.all_styles_mut(|style| style.animation_time = 0.0);
        panel.frame(Vec::new());
        let section = panel
            .ctx
            .data_mut(|d| d.get_temp::<egui::Id>(egui::Id::new("style-section-id")))
            .expect("Style id");
        panel.ctx.memory_mut(|m| m.request_focus(section));
        panel.key(egui::Key::Space, egui::Modifiers::NONE);
        panel.frame(Vec::new());
        panel.frame(Vec::new());
        let shown = panel
            .ctx
            .data_mut(|d| d.get_temp::<Vec<String>>(egui::Id::new("style-rules-shown")))
            .expect("the Style section drew its rule list");
        assert_eq!(shown, ["rule Button", "rule [hot] :hover"]);
    }

    thread_local! {
        /// The hierarchy rows the last frame laid out, by node path.
        pub(super) static TREE_ROWS: std::cell::RefCell<Vec<String>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    /// A 5 000-node tree with component instances (rows of varying height: an instance lists its
    /// inherited nodes) lays out only the rows in view; after scrolling, a click selects the
    /// clicked node and a drag still reorders (ADR-7326).
    #[test]
    fn a_five_thousand_node_tree_lays_out_the_rows_in_view_and_a_click_after_scrolling_selects() {
        let dir = regia_test_utils::unique_temp_dir("ui-designer-panel-large-tree");
        let ui_dir = dir.join("assets/ui");
        std::fs::create_dir_all(&ui_dir).expect("mkdir");
        std::fs::write(
            ui_dir.join("bar.ui.ron"),
            r#"(version: 9, component: (params: []), root: (name: "Bar", children: [(name: "Label"), (name: "Icon"), (name: "Fill")]))"#,
        )
        .expect("write component");
        let children: Vec<String> = (0..5_000)
            .map(|i| {
                if i % 5 == 0 {
                    format!(r#"(name: "inst{i}", use: "ui/bar.ui.ron")"#)
                } else {
                    format!(r#"(name: "n{i}")"#)
                }
            })
            .collect();
        let hud = ui_dir.join("hud.ui.ron");
        std::fs::write(
            &hud,
            format!(
                r#"(version: 9, root: (name: "Hud", children: [{}]))"#,
                children.join(", ")
            ),
        )
        .expect("write hud");
        let mut designer = UiDesigner::default();
        designer.open(&hud).expect("open");
        let mut panel = Panel {
            ctx: egui::Context::default(),
            designer,
            state: UiDesignerPanelState::default(),
            runtime: Default::default(),
        };
        let drawn = |panel: &mut Panel, events: Vec<egui::Event>| {
            panel.frame(events);
            TREE_ROWS.with(|rows| rows.borrow().clone())
        };
        drawn(&mut panel, Vec::new());
        let rows = drawn(&mut panel, Vec::new());
        assert_eq!(rows.first().map(String::as_str), Some("/"), "the root row first");
        assert!(rows.len() < 100, "a screenful of 5 001 rows: {}", rows.len());

        let inside = panel.rect("/0").center();
        panel.hover(inside);
        panel.frame(vec![egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -8_000.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        }]);
        let mut rows = Vec::new();
        for _ in 0..120 {
            rows = drawn(&mut panel, Vec::new());
        }
        let first: usize = rows
            .first()
            .and_then(|key| key.strip_prefix('/'))
            .and_then(|index| index.parse().ok())
            .expect("a child row in view");
        assert!(first > 50, "scrolled past the top: {first}");
        assert!(rows.len() < 100, "still a screenful: {}", rows.len());

        // Click a row in the middle of the view: that node is selected.
        let key = rows[rows.len() / 2].clone();
        let index: usize = key[1..].parse().expect("a child row");
        panel.click(panel.rect(&key).center());
        assert_eq!(panel.doc().selection, Some(vec![index]), "the clicked row {key}");

        // Drag that row onto the lower part of the row below the next one: one move.
        let undo_len = panel.doc().undo_len();
        let rows = drawn(&mut panel, Vec::new());
        let at = rows.iter().position(|k| *k == key).expect("still drawn");
        let target_key = rows[at + 2].clone();
        let from = panel.rect(&key).center();
        let target = panel.rect(&target_key);
        let to = egui::pos2(target.center().x, target.top() + target.height() * 0.9);
        panel.hover(from);
        panel.button(from, true);
        panel.hover(from + egui::vec2(0.0, 12.0));
        panel.hover(to);
        panel.hover(to);
        panel.button(to, false);
        panel.frame(Vec::new());
        assert_eq!(panel.doc().undo_len(), undo_len + 1, "the drop is one move");
        let moved = &panel.doc().file().root.children[index + 2];
        assert_eq!(
            moved.name.as_deref(),
            Some(node_name(index).as_str()),
            "it lands after the target"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn node_name(i: usize) -> String {
        if i.is_multiple_of(5) {
            format!("inst{i}")
        } else {
            format!("n{i}")
        }
    }

    /// The panel draws headless over a document without panicking, and a frame with no
    /// interaction changes nothing.
    #[test]
    fn panel_draws_headless() {
        let mut designer = UiDesigner::default();
        designer.new_document();
        let doc = designer.doc_mut().expect("doc");
        doc.apply(DesignOp::Add {
            parent: Vec::new(),
            index: None,
            node: Box::new(preset("button").expect("preset")),
        })
        .expect("add");
        let revision = doc.revision();
        let mut state = UiDesignerPanelState::default();
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            draw(ui, &mut designer, &mut state, &mut Default::default(), None, None);
        });
        // No renderer here: the font atlas upload is dropped on purpose.
        output.textures_delta.clear();
        assert_eq!(designer.doc().expect("doc").revision(), revision);
        assert!(state.cached.is_some(), "the preview was laid out");
    }

    #[test]
    fn errors_stay_on_screen_until_dismissed() {
        let mut panel = Panel::new();
        panel.designer.error = Some("bad_value: nope".to_owned());
        for _ in 0..3 {
            panel.frame(Vec::new());
        }
        assert_eq!(panel.designer.error.as_deref(), Some("bad_value: nope"));
    }

    /// A field edits the document, not a copy rebuilt each frame.
    #[test]
    fn typing_into_binding_rows_edits_the_document() {
        let mut panel = Panel::new();
        panel.type_into("source", "HudVm.health");
        panel.type_into("format", "{:.1}");
        let doc = panel.doc();
        let bind = &doc.file().root.children[0].bindings[0];
        assert_eq!(bind.source, "HudVm.health");
        assert_eq!(bind.format.as_deref(), Some("{:.1}"));
        assert_eq!(doc.undo_len(), 6, "three adds, the bind, one step per field");
    }

    /// ADR-7751: a theme-token binding shows a "theme token" row in place of the field row; typing
    /// into it is one undo step and the binding stays a theme binding (no field written).
    #[test]
    fn a_theme_binding_row_edits_the_token() {
        let mut panel = Panel::new();
        panel
            .doc()
            .apply(DesignOp::Set {
                node: vec![0],
                key: "bindings".to_owned(),
                value: json!([{ "theme": "color.accent", "target": "value" }]),
            })
            .expect("theme binding");
        panel.frame(Vec::new());
        let undo = panel.doc().undo_len();
        panel.type_into("theme token", "size.control.md");
        let doc = panel.doc();
        let bind = &doc.file().root.children[0].bindings[0];
        assert_eq!(bind.theme.as_deref(), Some("size.control.md"));
        assert!(bind.source.is_empty() && bind.animation.is_none(), "{bind:?}");
        assert_eq!(doc.undo_len(), undo + 1);
    }

    /// ADR-3148: the inspector's picture rows name an image by asset path or icon (one undo step
    /// each), set nine-slice insets through the same edit, and an empty name removes the image.
    #[test]
    fn picture_rows_name_the_selected_image() {
        let mut panel = Panel::new();
        panel
            .doc()
            .apply(DesignOp::Add {
                parent: Vec::new(),
                index: None,
                node: Box::new(preset("image").expect("preset")),
            })
            .expect("add");
        panel.doc().select(Some(vec![3])).expect("select");
        panel.frame(Vec::new());
        let undo = panel.doc().undo_len();
        panel.type_into("image asset", "assets/ui/panel.png");
        let image = |panel: &mut Panel| node_at(&panel.doc().file().root, &[3]).and_then(|n| n.image.clone());
        let picked = image(&mut panel).expect("image");
        assert_eq!(
            (picked.id, picked.asset.as_deref()),
            (0, Some("assets/ui/panel.png"))
        );
        assert_eq!(panel.doc().undo_len(), undo + 1);
        panel.type_into("image icon", "gear");
        let icon = image(&mut panel).expect("image");
        assert_eq!(
            (icon.asset.as_deref(), icon.icon.as_deref()),
            (None, Some("gear"))
        );
        panel.type_into("mask icon", "ring");
        let mask = node_at(&panel.doc().file().root, &[3])
            .and_then(|n| n.mask.clone())
            .expect("mask");
        assert_eq!(mask.icon.as_deref(), Some("ring"));
        // Clearing the icon name removes the image.
        panel.reveal("image icon");
        panel.click(panel.rect("image icon").center());
        panel.key(egui::Key::A, egui::Modifiers::COMMAND);
        panel.key(egui::Key::Backspace, egui::Modifiers::NONE);
        panel.key(egui::Key::Enter, egui::Modifiers::NONE);
        panel.frame(Vec::new());
        panel.frame(Vec::new());
        assert_eq!(image(&mut panel), None, "an empty name removes the image");
    }

    /// A localized node's `text` and `loc key` fields are editable until a locale other than the source
    /// previews, then read-only (no editable field is drawn); Source unlocks them again (ADR-7855).
    #[test]
    fn a_localized_nodes_text_fields_lock_while_a_translation_previews() {
        let mut panel =
            Panel::with_text(r#"(version: 11, root: (name: "title", text: (spans: [Loc("menu-start")])))"#);
        panel.frame(Vec::new());
        let editable = |panel: &Panel| {
            panel
                .ctx
                .data_mut(|d| d.get_temp::<egui::Rect>(egui::Id::new(("ui-designer-test-rect", "loc key"))))
                .is_some()
        };
        assert!(editable(&panel), "the key row is a field while nothing previews");
        let mut world = World::new();
        world.insert_resource(crate::loc_panel::preview::PreviewLocale {
            locale: Some("fr".to_owned()),
        });
        crate::loc_panel::preview::sync_frame(&panel.ctx, &mut world, None);
        panel
            .ctx
            .data_mut(|d| d.remove::<egui::Rect>(egui::Id::new(("ui-designer-test-rect", "loc key"))));
        panel.frame(Vec::new());
        assert!(
            !editable(&panel),
            "a translation previews: the key row is read-only"
        );
        world.insert_resource(crate::loc_panel::preview::PreviewLocale::default());
        crate::loc_panel::preview::sync_frame(&panel.ctx, &mut world, None);
        panel.frame(Vec::new());
        assert!(editable(&panel), "Source unlocks it");
    }

    /// A drag on a nine-slice inset runs many frames, each `changed`: it is one undo step, as the
    /// picture rows promise (it used to push one step per frame).
    #[test]
    fn a_nine_slice_inset_drag_is_one_undo_step() {
        let mut panel = Panel::new();
        let mut image = preset("image").expect("preset");
        if let Some(def) = &mut image.image {
            def.nine_slice = Some((8.0, 8.0, 8.0, 8.0));
        }
        panel
            .doc()
            .apply(DesignOp::Add {
                parent: Vec::new(),
                index: None,
                node: Box::new(image),
            })
            .expect("add");
        panel.doc().select(Some(vec![3])).expect("select");
        panel.frame(Vec::new());
        let undo = panel.doc().undo_len();
        panel.reveal("nine-slice inset");
        let from = panel.rect("nine-slice inset").center();
        panel.hover(from);
        panel.button(from, true);
        for step in 1..=6 {
            panel.hover(from + egui::vec2(10.0 * step as f32, 0.0));
        }
        panel.button(from + egui::vec2(60.0, 0.0), false);
        panel.frame(Vec::new());
        let insets = node_at(&panel.doc().file().root, &[3])
            .and_then(|n| n.image.clone())
            .and_then(|i| i.nine_slice)
            .expect("insets");
        assert!(insets.3 > 8.0, "the drag moved the bottom inset: {insets:?}");
        assert_eq!(panel.doc().undo_len(), undo + 1, "one drag, one undo step");
        // The next edit is its own step again.
        panel.doc().undo().expect("undo");
        let back = node_at(&panel.doc().file().root, &[3])
            .and_then(|n| n.image.clone())
            .and_then(|i| i.nine_slice);
        assert_eq!(
            back,
            Some((8.0, 8.0, 8.0, 8.0)),
            "one undo returns to before the drag"
        );
    }

    #[test]
    fn a_sample_changes_the_preview_not_the_document() {
        let mut panel = Panel::new();
        let revision = panel.doc().revision();
        panel.type_into("sample", "0.25");
        let doc = panel.doc();
        assert_eq!(
            doc.preview.samples.get("Model.field").map(String::as_str),
            Some("0.25")
        );
        assert_eq!(doc.revision(), revision);
    }

    #[test]
    fn undo_and_redo_shortcuts_follow_the_pointer() {
        let mut panel = Panel::new();
        let undo_len = panel.doc().undo_len();
        let command = egui::Modifiers::COMMAND;

        // The pointer is elsewhere: the key stays the shell's.
        panel.key(egui::Key::Z, command);
        assert_eq!(panel.doc().undo_len(), undo_len);

        panel.hover(egui::pos2(800.0, 500.0));
        panel.key(egui::Key::Z, command);
        assert_eq!(panel.doc().undo_len(), undo_len - 1, "undo");
        panel.key(egui::Key::Z, command | egui::Modifiers::SHIFT);
        assert_eq!(panel.doc().undo_len(), undo_len, "Ctrl+Shift+Z redoes");
        panel.key(egui::Key::Z, command);
        panel.key(egui::Key::Y, command);
        assert_eq!(panel.doc().undo_len(), undo_len, "Ctrl+Y redoes");
    }

    /// A row dragged onto the lower quarter of another lands after it, as one undo step.
    #[test]
    fn dragging_a_row_reorders_the_tree() {
        let mut panel = Panel::new();
        let undo_len = panel.doc().undo_len();
        let bound = |doc: &mut UiDesign, at: usize| !doc.file().root.children[at].bindings.is_empty();
        assert!(bound(panel.doc(), 0));
        let from = panel.rect("/0").center();
        let target = panel.rect("/2");
        let to = egui::pos2(target.center().x, target.top() + target.height() * 0.9);
        panel.hover(from);
        panel.button(from, true);
        panel.hover(from + egui::vec2(0.0, 12.0));
        panel.hover(to);
        panel.hover(to);
        panel.button(to, false);
        panel.frame(Vec::new());
        assert!(bound(panel.doc(), 2), "the bound slider is last now");
        assert_eq!(panel.doc().undo_len(), undo_len + 1);
    }

    /// What the layout rows send decodes to the same style and survives a save.
    #[test]
    fn layout_row_values_round_trip_through_the_format() {
        let mut panel = Panel::new();
        let doc = panel.doc();
        let mut set = |key: &str, value: Value| {
            doc.apply(DesignOp::Set {
                node: vec![1],
                key: key.to_owned(),
                value,
            })
            .expect(key);
        };
        let size = [Dimension::Points(120.0), Dimension::Percent(50.0)];
        let margin = Edges {
            left: Dimension::Points(1.0),
            top: Dimension::Auto,
            right: Dimension::Percent(3.0),
            bottom: Dimension::Points(4.0),
        };
        let color = BackgroundDef::Color(UiColor([0.1, 0.2, 0.3, 0.5]));
        set("style.size", serde_json::to_value(size).expect("json"));
        set("style.margin", serde_json::to_value(margin).expect("json"));
        set("background", serde_json::to_value(&color).expect("json"));
        set("style.justify_content", json!("spacebetween"));
        set("opacity", json!(0.5));
        let doc = panel.doc();
        let node = &doc.file().root.children[1];
        assert_eq!(node.style.size, size);
        assert_eq!(node.style.margin, margin);
        assert_eq!(node.style.justify_content, Justify::SpaceBetween);
        assert_eq!(node.background, Some(color));
        let text = write_ui_ron(doc.file()).expect("write");
        let back = parse_ui_ron(&text, "back.ui.ron", &UiMigrators::default()).expect("parse");
        assert_eq!(&back, doc.file());
    }

    /// Drags a palette chip to a point, holding long enough for egui to see the drag.
    fn drag_chip(panel: &mut Panel, chip: &str, to: egui::Pos2) {
        let from = panel.rect(&format!("palette:{chip}")).center();
        panel.hover(from);
        panel.button(from, true);
        panel.hover(from + egui::vec2(0.0, 12.0));
        panel.hover(to);
        panel.hover(to);
        panel.button(to, false);
        panel.frame(Vec::new());
    }

    #[test]
    fn dragging_a_palette_chip_onto_a_row_adds_the_node_there() {
        let mut panel = Panel::new();
        let undo_len = panel.doc().undo_len();
        let target = panel.rect("/1");
        let into = egui::pos2(target.center().x, target.center().y);
        drag_chip(&mut panel, "button", into);
        let doc = panel.doc();
        let child = &doc.file().root.children[1].children;
        assert_eq!(child.len(), 1, "a child of the slider");
        assert_eq!(node_kind(&child[0]), "button");
        assert_eq!(doc.undo_len(), undo_len + 1, "one undo step");
        assert_eq!(doc.selection, Some(vec![1, 0]), "the new node is selected");
        doc.undo().expect("undo");
        assert!(doc.file().root.children[1].children.is_empty());
    }

    #[test]
    fn dragging_a_palette_chip_beside_a_row_adds_a_sibling() {
        let mut panel = Panel::new();
        let target = panel.rect("/0");
        let after = egui::pos2(target.center().x, target.top() + target.height() * 0.9);
        drag_chip(&mut panel, "text", after);
        let doc = panel.doc();
        assert_eq!(doc.file().root.children.len(), 4);
        assert_eq!(node_kind(&doc.file().root.children[1]), "text");
    }

    #[test]
    fn dragging_a_palette_chip_onto_the_preview_adds_to_the_node_under_it() {
        let mut panel = Panel::new();
        // Room to aim at: the middle slider gets a tall rect.
        panel
            .doc()
            .apply(DesignOp::Set {
                node: vec![1],
                key: "style.size".to_owned(),
                value: json!([800, 400]),
            })
            .expect("size");
        panel.frame(Vec::new());
        let target = panel.rect("preview:/1");
        drag_chip(&mut panel, "progress", target.center());
        let doc = panel.doc();
        let children = &doc.file().root.children[1].children;
        assert_eq!(children.len(), 1, "into the node under the pointer");
        assert_eq!(node_kind(&children[0]), "progress");
    }

    #[test]
    fn a_chip_dropped_on_nothing_adds_nothing() {
        let mut panel = Panel::new();
        let revision = panel.doc().revision();
        drag_chip(&mut panel, "panel", egui::pos2(5.0, 995.0));
        assert_eq!(panel.doc().revision(), revision);
    }

    #[test]
    fn typing_a_repeat_source_makes_the_node_a_template_and_the_preview_shows_rows() {
        let mut panel = Panel::new();
        panel
            .doc()
            .apply(DesignOp::Add {
                parent: vec![0],
                index: None,
                node: Box::new(preset("text").expect("text")),
            })
            .expect("child");
        panel.doc().select(Some(vec![0])).expect("select");
        panel.frame(Vec::new());
        panel.type_into("repeat source", "Vm.items");
        panel.type_into("repeat key", "id");
        let doc = panel.doc();
        let repeat = doc.file().root.children[0].repeat.clone().expect("repeat");
        assert_eq!(repeat.source, "Vm.items");
        assert_eq!(repeat.key.as_deref(), Some("id"));
        panel.frame(Vec::new());
        let (_, _, frame) = panel.state.cached.as_ref().expect("preview");
        let rows = frame.nodes.iter().filter(|n| n.path == "/0/0").count();
        assert_eq!(rows, crate::ui_designer_preview::DEFAULT_REPEAT_COUNT);
    }

    #[test]
    fn a_converter_edit_is_one_binding_change_and_table_text_round_trips() {
        let mut panel = Panel::new();
        let converter = BindConverter::EnumText {
            table: vec![("Idle".to_owned(), "Waiting".to_owned())],
            default: None,
        };
        panel
            .doc()
            .apply(DesignOp::Set {
                node: vec![0],
                key: "bindings".to_owned(),
                value: json!([{ "source": "Model.field", "target": "text", "converter": converter }]),
            })
            .expect("bind");
        panel.frame(Vec::new());
        panel.type_into("default", "?");
        panel.type_into("table", "Idle=Waiting; Run=On the way; bad");
        let bind = &panel.doc().file().root.children[0].bindings[0];
        assert_eq!(
            bind.converter,
            Some(BindConverter::EnumText {
                table: vec![
                    ("Idle".to_owned(), "Waiting".to_owned()),
                    ("Run".to_owned(), "On the way".to_owned()),
                ],
                default: Some("?".to_owned()),
            })
        );
        let table = [
            ("a".to_owned(), "b".to_owned()),
            ("c".to_owned(), "d e".to_owned()),
        ];
        assert_eq!(parse_table(&format_table(&table)), table);
    }

    /// The sample row of an item binding writes the key every sample row shares.
    #[test]
    fn an_item_binding_sample_uses_the_item_path() {
        let mut panel = Panel::new();
        let doc = panel.doc();
        doc.apply(DesignOp::Add {
            parent: vec![0],
            index: None,
            node: Box::new(preset("text").expect("text")),
        })
        .expect("child");
        doc.apply(crate::ui_designer::repeat_op(
            vec![0],
            Some("Vm.items"),
            Some("id"),
        ))
        .expect("repeat");
        doc.apply(DesignOp::Set {
            node: vec![0, 0],
            key: "bindings".to_owned(),
            value: json!([{ "source": ".name" }]),
        })
        .expect("bind");
        doc.select(Some(vec![0, 0])).expect("select");
        panel.frame(Vec::new());
        panel.type_into("sample", "Sword");
        assert_eq!(
            panel
                .doc()
                .preview
                .samples
                .get("Vm.items[].name")
                .map(String::as_str),
            Some("Sword")
        );
        panel.frame(Vec::new());
        let (_, _, frame) = panel.state.cached.as_ref().expect("preview");
        let texts: Vec<&str> = frame
            .nodes
            .iter()
            .filter(|n| n.path == "/0/0")
            .filter_map(|n| n.text.as_deref())
            .collect();
        assert_eq!(texts, ["Sword", "Sword", "Sword"]);
    }

    /// With *Themed* on, the preview shows the runtime renderer's pixels as a texture of the
    /// preview size, drawn once per document revision and settings, not once per frame.
    #[test]
    fn themed_preview_draws_once_per_change() {
        let device = match crate::render::headless_device() {
            Ok(device) => device,
            Err(e) => {
                eprintln!("skipped: no GPU device ({e})");
                return;
            }
        };
        let mut panel = Panel::new();
        panel.doc().preview.size = [320.0, 200.0];
        panel.doc().preview.safe_area = [0.0; 4];
        panel.state.themed = true;
        panel.state.themed_inputs = Some((device, ThemeChoice::default()));
        panel.frame(Vec::new());
        assert!(
            panel.state.themed_error.is_none(),
            "{:?}",
            panel.state.themed_error
        );
        let (rev, _, texture) = panel.state.themed_cache.as_ref().expect("themed frame");
        assert_eq!(texture.size(), [320, 200]);
        let (rev, id) = (*rev, texture.id());
        panel.frame(Vec::new());
        let (_, _, again) = panel.state.themed_cache.as_ref().expect("themed frame");
        assert_eq!(again.id(), id, "no redraw without a change");

        panel.doc().preview.size = [240.0, 160.0];
        panel.frame(Vec::new());
        let (now, settings, texture) = panel.state.themed_cache.as_ref().expect("themed frame");
        assert_eq!((*now, settings.size), (rev, [240.0, 160.0]));
        assert_eq!(texture.size(), [240, 160]);

        panel.state.themed = false;
        panel.frame(Vec::new());
        assert!(panel.state.themed_cache.is_none(), "off drops the texture");
    }

    /// Spec 111 § 6 (spec conformance 03-11): the palette lists the project's components (only
    /// documents with a `component:` declaration), clicking one adds an instance under the
    /// selection in one undo step, and the hierarchy lists the instance's inherited nodes.
    #[test]
    fn a_component_chip_adds_an_instance_and_the_tree_lists_its_inherited_nodes() {
        let dir = regia_test_utils::unique_temp_dir("ui-designer-panel-components");
        let ui_dir = dir.join("assets/ui");
        std::fs::create_dir_all(&ui_dir).expect("mkdir");
        std::fs::write(
            ui_dir.join("bar.ui.ron"),
            r#"(version: 9, component: (params: [(name: "label", type: string, default: "HP", targets: [(node: "Label", property: text)])]), root: (name: "Bar", children: [(name: "Label", text: (spans: ["HP"]))]))"#,
        )
        .expect("write component");
        let hud = ui_dir.join("hud.ui.ron");
        std::fs::write(&hud, r#"(version: 9, root: (name: "Hud"))"#).expect("write hud");
        let mut designer = UiDesigner::default();
        designer.open(&hud).expect("open");
        let mut panel = Panel {
            ctx: egui::Context::default(),
            designer,
            state: UiDesignerPanelState::default(),
            runtime: Default::default(),
        };
        panel.frame(Vec::new());
        let listed: Vec<String> = panel
            .state
            .components
            .as_ref()
            .map(|(_, c)| c.iter().map(|i| i.path.clone()).collect())
            .unwrap_or_default();
        assert_eq!(listed, ["ui/bar.ui.ron"], "a plain document is not a component");

        let undo_len = panel.doc().undo_len();
        panel.click(panel.rect("component:ui/bar.ui.ron").center());
        let doc = panel.doc();
        assert_eq!(doc.undo_len(), undo_len + 1, "one undo step");
        let children = &doc.file().root.children;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].use_component.as_deref(), Some("ui/bar.ui.ron"));
        assert_eq!(children[0].name.as_deref(), Some("bar"));
        assert!(children[0].children.is_empty(), "the source keeps a reference");

        panel.frame(Vec::new());
        let rows = panel
            .state
            .inherited
            .as_ref()
            .map(|(_, rows)| rows.clone())
            .unwrap_or_default();
        let (path, nodes, diagnostics) = rows
            .iter()
            .find(|(p, _, _)| p.as_slice() == [0])
            .expect("the instance has inherited rows");
        assert_eq!(path.as_slice(), [0]);
        assert!(
            nodes.iter().skip(1).any(|(_, label)| label.contains("Label")),
            "the component's own nodes are listed under the instance: {nodes:?}"
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two absolutely placed panels (`#a`, `#b`).
    const ABSOLUTE_DOC: &str = r#"(
    version: 11,
    root: (
        style: (size: (Percent(100), Percent(100))),
        children: [
            (name: "a", style: (position: absolute, inset: (50, 60, Auto, Auto), size: (300, 200))),
            (name: "b", style: (position: absolute, inset: (900, 60, Auto, Auto), size: (300, 200))),
        ],
    ),
)
"#;

    fn inset_left_top(panel: &mut Panel, at: usize) -> (Dimension, Dimension) {
        let inset = panel.doc().file().root.children[at].style.inset;
        (inset.left, inset.top)
    }

    /// Spec 81 § 4.7: dragging a node in the preview writes its inset live, as one undo step, by the
    /// pointer's travel in frame px; Ctrl+Z restores it.
    #[test]
    fn dragging_a_preview_node_moves_it_in_one_undo_step() {
        let mut panel = Panel::with_text(ABSOLUTE_DOC);
        panel.state.no_snap = true;
        panel.frame(Vec::new());
        let node = panel.rect("preview:/0");
        let frame_rect = panel
            .state
            .cached
            .as_ref()
            .and_then(|(_, _, f)| rect_of(f, &[0]))
            .expect("laid out");
        let k = node.width() / frame_rect[2];
        let from = node.center();
        let to = from + egui::vec2(40.0, 20.0);
        panel.hover(from);
        panel.button(from, true);
        panel.hover(from + egui::vec2(10.0, 5.0));
        panel.hover(to);
        panel.hover(to);
        assert!(panel.state.drag.is_some(), "a drag runs");
        panel.button(to, false);
        panel.frame(Vec::new());
        assert!(panel.state.drag.is_none() && panel.state.guides.is_empty());
        let (left, top) = inset_left_top(&mut panel, 0);
        let (Dimension::Points(l), Dimension::Points(t)) = (left, top) else {
            panic!("points: {left:?} {top:?}");
        };
        assert!((l - 50.0 - 40.0 / k).abs() < 0.6, "left {l} (zoom {k})");
        assert!((t - 60.0 - 20.0 / k).abs() < 0.6, "top {t} (zoom {k})");
        assert_eq!(panel.doc().undo_len(), 1, "one undo step for the whole drag");
        panel.hover(egui::pos2(10.0, 900.0));
        panel.key(egui::Key::Z, egui::Modifiers::COMMAND);
        assert_eq!(
            inset_left_top(&mut panel, 0),
            (Dimension::Points(50.0), Dimension::Points(60.0))
        );
    }

    /// ADR-9980, spec 81 § 4.7: with three nodes selected, dragging one moves all three by the same
    /// delta as one undo step; Ctrl+Z restores all three.
    #[test]
    fn dragging_a_selected_node_moves_the_whole_selection_in_one_undo_step() {
        let doc = ABSOLUTE_DOC.replace(
            "        ],\n",
            "            (name: \"c\", style: (position: absolute, inset: (50, 400, Auto, Auto), size: (300, 200))),\n        ],\n",
        );
        let mut panel = Panel::with_text(&doc);
        panel.state.no_snap = true;
        panel
            .doc()
            .select_many(vec![vec![0], vec![1], vec![2]])
            .expect("select three");
        panel.frame(Vec::new());
        let node = panel.rect("preview:/0");
        let frame_rect = panel
            .state
            .cached
            .as_ref()
            .and_then(|(_, _, f)| rect_of(f, &[0]))
            .expect("laid out");
        let k = node.width() / frame_rect[2];
        let from = node.center();
        let to = from + egui::vec2(40.0, 20.0);
        panel.hover(from);
        panel.button(from, true);
        panel.hover(from + egui::vec2(10.0, 5.0));
        panel.hover(to);
        panel.hover(to);
        panel.button(to, false);
        panel.frame(Vec::new());
        let starts = [(50.0, 60.0), (900.0, 60.0), (50.0, 400.0)];
        for (at, (l0, t0)) in starts.into_iter().enumerate() {
            let (left, top) = inset_left_top(&mut panel, at);
            let (Dimension::Points(l), Dimension::Points(t)) = (left, top) else {
                panic!("points: {left:?} {top:?}");
            };
            assert!((l - l0 - 40.0 / k).abs() < 0.6, "node {at}: left {l} (zoom {k})");
            assert!((t - t0 - 20.0 / k).abs() < 0.6, "node {at}: top {t} (zoom {k})");
        }
        assert_eq!(panel.doc().undo_len(), 1, "one undo step for the group drag");
        assert_eq!(panel.doc().selected_nodes().len(), 3, "the selection is kept");
        panel.hover(egui::pos2(10.0, 900.0));
        panel.key(egui::Key::Z, egui::Modifiers::COMMAND);
        for (at, (l0, t0)) in starts.into_iter().enumerate() {
            assert_eq!(
                inset_left_top(&mut panel, at),
                (Dimension::Points(l0), Dimension::Points(t0)),
                "node {at} restored"
            );
        }
    }

    /// A resize handle of the selected node resizes it; with snapping, its edge lands on the
    /// sibling's edge.
    #[test]
    fn a_handle_resizes_and_snaps_to_the_sibling() {
        let mut panel = Panel::with_text(ABSOLUTE_DOC);
        panel.doc().select(Some(vec![0])).expect("select");
        panel.frame(Vec::new());
        let handle = panel.rect("handle:E").center();
        let b = panel.rect("preview:/1");
        // Drag a's right edge to 2 screen points short of b's left edge.
        let to = egui::pos2(b.left() - 2.0, handle.y);
        panel.hover(handle);
        panel.button(handle, true);
        panel.hover(handle + egui::vec2(8.0, 0.0));
        panel.hover(to);
        panel.hover(to);
        panel.button(to, false);
        panel.frame(Vec::new());
        let size = panel.doc().file().root.children[0].style.size[0];
        // a starts at 50 and b at 900: snapped, a is 850 wide.
        assert_eq!(
            size,
            Dimension::Points(850.0),
            "the right edge snapped onto b's left edge"
        );
        assert_eq!(panel.doc().undo_len(), 1);
    }

    /// Ctrl+click in the hierarchy adds to the selection; Ctrl+D duplicates every selected node as
    /// one undo step, Delete removes them as one more.
    #[test]
    fn ctrl_click_selects_many_and_ctrl_d_duplicates_them() {
        let mut panel = Panel::new();
        let undo = panel.doc().undo_len();
        panel.click(panel.rect("/0").center());
        let at = panel.rect("/2").center();
        panel.hover(at);
        panel.frame_with(
            vec![egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::COMMAND,
            }],
            egui::Modifiers::COMMAND,
        );
        panel.frame_with(
            vec![egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::COMMAND,
            }],
            egui::Modifiers::COMMAND,
        );
        panel.frame(Vec::new());
        assert_eq!(panel.doc().selected_nodes(), vec![vec![0], vec![2]]);
        panel.key(egui::Key::D, egui::Modifiers::COMMAND);
        assert_eq!(panel.doc().file().root.children.len(), 5);
        assert_eq!(panel.doc().undo_len(), undo + 1);
        assert_eq!(
            panel.doc().selected_nodes(),
            vec![vec![1], vec![4]],
            "the copies are selected"
        );
        panel.key(egui::Key::Delete, egui::Modifiers::NONE);
        assert_eq!(panel.doc().file().root.children.len(), 3);
        assert_eq!(panel.doc().undo_len(), undo + 2);
    }

    /// Copy in one document and paste in the next: the designer's clipboard outlives the document,
    /// and the pasted slider keeps its binding.
    #[test]
    fn copy_and_paste_between_documents_in_the_panel() {
        let mut panel = Panel::new();
        panel.doc().select(Some(vec![0])).expect("select");
        let over = panel.rect("/0").center();
        panel.hover(over);
        panel.frame(vec![egui::Event::Copy]);
        assert!(
            panel
                .designer
                .clipboard
                .as_deref()
                .is_some_and(|t| t.contains("Model.field"))
        );
        panel.designer.new_document();
        panel.frame(Vec::new());
        panel.hover(over);
        panel.frame(vec![egui::Event::Paste("not node json".to_owned())]);
        let root = &panel.doc().file().root;
        assert_eq!(root.children.len(), 1, "pasted from the designer's clipboard");
        assert_eq!(root.children[0].bindings[0].source, "Model.field");
        assert_eq!(panel.doc().undo_len(), 1);
    }

    /// The typed widget rows edit one field of the widget (one undo step), the same edit the agent's
    /// `widget.<field>` key makes.
    #[test]
    fn typed_widget_rows_edit_the_widget() {
        let mut panel = Panel::new();
        panel
            .doc()
            .apply(DesignOp::Add {
                parent: Vec::new(),
                index: None,
                node: Box::new(preset("text_input").expect("preset")),
            })
            .expect("add");
        panel.frame(Vec::new());
        let undo = panel.doc().undo_len();
        panel.type_into("placeholder", "Your name");
        let widget = panel.doc().file().root.children[3].widget.clone();
        assert_eq!(
            widget,
            Some(regia_ui::authoring::WidgetDef::TextInput(
                "Your name".to_owned(),
                false,
                Default::default()
            ))
        );
        assert_eq!(panel.doc().undo_len(), undo + 1);
        // `.ui.ron` v11 (ADR-9980): the mask character has its own row.
        panel.type_into("secret", "*");
        let Some(regia_ui::authoring::WidgetDef::TextInput(_, _, options)) =
            panel.doc().file().root.children[3].widget.clone()
        else {
            panic!("a text input");
        };
        assert_eq!(options.secret, Some('*'));
        assert_eq!(panel.doc().undo_len(), undo + 2);
    }

    /// Spec 81 § 4.7: the wheel zooms the canvas about the pointer (the frame point under it stays
    /// there), and Fit and 100 % reset the view. The view is panel state: the document is untouched.
    #[test]
    fn the_wheel_zooms_about_the_pointer_and_fit_resets() {
        let mut panel = Panel::with_text(ABSOLUTE_DOC);
        let revision = panel.doc().revision();
        let canvas = panel.rect("canvas");
        let size = panel
            .state
            .cached
            .as_ref()
            .map(|(_, _, f)| f.size)
            .expect("laid out");
        let avail = [canvas.width(), canvas.height()];
        let at = canvas.min + egui::vec2(canvas.width() * 0.3, canvas.height() * 0.4);
        let local = [at.x - canvas.min.x, at.y - canvas.min.y];
        let before = panel.state.view.to_frame(avail, size, local);
        panel.hover(at);
        panel.frame(vec![egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, 120.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        }]);
        let fit = CanvasView::fit_zoom(avail, size);
        let zoom = panel.state.view.zoom.expect("zoomed");
        assert!(zoom > fit * 1.2, "zoomed in once: {zoom} vs fit {fit}");
        let after = panel.state.view.to_canvas(avail, size, before);
        assert!(
            (after[0] - local[0]).abs() < 0.01 && (after[1] - local[1]).abs() < 0.01,
            "the point under the pointer stays: {after:?} vs {local:?}"
        );
        panel.click(panel.rect("view:100").center());
        assert_eq!(panel.state.view.zoom, Some(1.0));
        panel.click(panel.rect("view:fit").center());
        assert_eq!(panel.state.view, CanvasView::default());
        assert_eq!(panel.doc().revision(), revision, "the view is not the document");
    }

    // ------------------------------------------------------------ w10 (ADR-7677)

    /// Calls the designer agent method `name` (the main table or the component table) on a world
    /// holding `designer`.
    fn agent(designer: UiDesigner, name: &str, params: Value) -> (UiDesigner, Value) {
        let mut world = World::new();
        world.insert_resource(designer);
        let table: Vec<_> = crate::ui_designer_agent::methods()
            .into_iter()
            .chain(crate::ui_designer_component_agent::methods())
            .collect();
        let (_, handler) = table.iter().find(|(s, _)| s.name == name).expect(name);
        let out = handler(&mut world, &params).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        (world.remove_resource::<UiDesigner>().expect("designer"), out)
    }

    /// The custom size and inset fields set the document's preview settings (its view state: no undo
    /// step, no revision, nothing in the file), equal to `editor.ui_designer_preview` with the same
    /// numbers; insets are limited to the window like the runtime's `SafeArea`, and a size that
    /// leaves no area is refused with nothing changed.
    #[test]
    fn custom_size_and_insets_match_the_agent_and_stay_out_of_the_file() {
        let mut panel = Panel::new();
        let file_before = write_ui_ron(panel.doc().file()).expect("write");
        let (rev, undo) = (panel.doc().revision(), panel.doc().undo_len());
        panel.type_into("preview width", "1000");
        panel.type_into("preview height", "700");
        panel.type_into("inset left", "10");
        panel.type_into("inset top", "20");
        panel.type_into("inset right", "30");
        panel.type_into("inset bottom", "40");
        assert!(panel.designer.error.is_none(), "{:?}", panel.designer.error);
        assert_eq!(panel.doc().preview.size, [1000.0, 700.0]);
        assert_eq!(panel.doc().preview.safe_area, [10.0, 20.0, 30.0, 40.0]);
        assert_eq!((panel.doc().revision(), panel.doc().undo_len()), (rev, undo));
        assert_eq!(
            write_ui_ron(panel.doc().file()).expect("write"),
            file_before,
            "view state only"
        );
        // The schematic preview lays out at the custom size.
        let frame = &panel.state.cached.as_ref().expect("frame").2;
        assert_eq!(frame.size, [1000.0, 700.0]);
        assert_eq!(frame.safe_area, [10.0, 20.0, 30.0, 40.0]);

        // The agent with the same numbers makes the same settings.
        let mut twin = UiDesigner::default();
        twin.open_text(&file_before, None).expect("open");
        let (mut twin, out) = agent(
            twin,
            "editor.ui_designer_preview",
            json!({ "size": [1000, 700], "safe_area": [10, 20, 30, 40] }),
        );
        assert_eq!(out["size"], json!([1000.0, 700.0]));
        assert_eq!(twin.doc_mut().expect("doc").preview, panel.doc().preview);

        // An inset past the window is limited to it (the runtime's clamp) and then refused: it
        // would leave no area; the settings stay.
        panel.type_into("inset left", "5000");
        assert!(
            panel
                .designer
                .error
                .as_deref()
                .is_some_and(|e| e.contains("leave some area")),
            "{:?}",
            panel.designer.error
        );
        assert_eq!(panel.doc().preview.safe_area, [10.0, 20.0, 30.0, 40.0]);
    }

    /// With Themed on, the theme picker lists the active theme, the built-ins and the project's
    /// theme files; a pick sets the document's `theme` setting, the same as the agent's `theme`
    /// (and with a GPU the themed frames draw the same pixels with that theme).
    #[test]
    fn the_theme_picker_sets_the_document_theme_like_the_agent() {
        let dir = regia_test_utils::unique_temp_dir("ui-designer-theme-picker");
        let themes = dir.join("assets/ui/themes");
        std::fs::create_dir_all(&themes).expect("mkdir");
        std::fs::write(
            themes.join("night.theme.ron"),
            r##"(format_version: 1, name: "night", extends: "regia-dark", tokens: {"color.accent": "#ff2040"})"##,
        )
        .expect("theme");
        let choice = ThemeChoice {
            files: project_theme_files(&dir),
            ..ThemeChoice::default()
        };
        let names = theme_names(&choice);
        assert!(names.iter().any(|n| n == "night"), "{names:?}");
        assert!(names.iter().any(|n| n == "regia-dark"), "{names:?}");

        let mut panel = Panel::new();
        panel.state.themed = true;
        panel.state.theme_names = Some((choice.files.clone(), names));
        panel.frame(Vec::new());
        panel.click(panel.rect("theme").center());
        panel.click(panel.rect("theme:night").center());
        assert_eq!(panel.doc().preview.theme.as_deref(), Some("night"));
        let text = write_ui_ron(panel.doc().file()).expect("write");
        assert!(
            !text.contains("night"),
            "the theme is view state, not file content"
        );

        let mut twin = UiDesigner::default();
        twin.open_text(&text, None).expect("open");
        let (mut twin, out) = agent(twin, "editor.ui_designer_preview", json!({ "theme": "night" }));
        assert_eq!(out["theme"], "night");
        assert_eq!(twin.doc_mut().expect("doc").preview, panel.doc().preview);

        panel.click(panel.rect("theme").center());
        panel.click(panel.rect("theme:active").center());
        assert_eq!(panel.doc().preview.theme, None);
        let (mut twin, _) = agent(twin, "editor.ui_designer_preview", json!({ "theme": null }));
        assert_eq!(twin.doc_mut().expect("doc").preview.theme, None);

        // Pixels: the same settings draw the same frame (needs a Vulkan device; lavapipe counts).
        if let Ok(device) = crate::render::headless_device() {
            let mut settings = panel.doc().preview.clone();
            settings.size = [160.0, 100.0];
            settings.safe_area = [0.0; 4];
            settings.theme = Some("night".to_owned());
            let file = panel.doc().file().clone();
            let a = themed_preview(&file, &settings, None, &choice, &device).expect("draws");
            let by_name = ThemeChoice {
                theme: Some("night".to_owned()),
                ..choice.clone()
            };
            settings.theme = None;
            let b = themed_preview(&file, &settings, None, &by_name, &device).expect("draws");
            assert_eq!(a.theme, "night");
            assert_eq!(a, b, "the document's pick draws what the agent's theme draws");
        } else {
            eprintln!("pixel check skipped: no GPU device");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An instance lists its component's slots: the picker sets a slot's content, a palette chip
    /// dropped on the slot row appends to it, Clear empties it; each is one undo step with the same
    /// bytes as `editor.ui_designer_set_instance_slot`.
    #[test]
    fn slot_rows_set_and_clear_slot_content_with_the_agent_bytes() {
        let dir = regia_test_utils::unique_temp_dir("ui-designer-panel-slots");
        let ui_dir = dir.join("assets/ui");
        std::fs::create_dir_all(&ui_dir).expect("mkdir");
        std::fs::write(
            ui_dir.join("card.ui.ron"),
            r#"(version: 11, component: (slots: ["badge"]), root: (name: "Card", children: [(name: "Badge", slot: "badge", children: [(name: "DefaultBadge")])]))"#,
        )
        .expect("write component");
        let hud = ui_dir.join("hud.ui.ron");
        std::fs::write(
            &hud,
            r#"(version: 11, root: (name: "Hud", children: [(use: "ui/card.ui.ron", name: "C1")]))"#,
        )
        .expect("write hud");
        let open = || {
            let mut designer = UiDesigner::default();
            designer.open(&hud).expect("open");
            designer
        };
        let mut panel = Panel {
            ctx: egui::Context::default(),
            designer: open(),
            state: UiDesignerPanelState::default(),
            runtime: Default::default(),
        };
        panel.doc().select(Some(vec![0])).expect("select");
        panel.frame(Vec::new());
        let original = write_ui_ron(panel.doc().file()).expect("write");
        let undo = panel.doc().undo_len();

        panel.click(panel.rect("slot:badge").center());
        panel.click(panel.rect("slot:badge:text").center());
        assert!(panel.designer.error.is_none(), "{:?}", panel.designer.error);
        assert_eq!(panel.doc().undo_len(), undo + 1, "one undo step");
        let content = panel.doc().file().root.children[0]
            .slots
            .get("badge")
            .cloned()
            .expect("filled");
        assert_eq!(content, vec![preset("text").expect("preset")]);
        let ours = write_ui_ron(panel.doc().file()).expect("write");
        let (mut twin, out) = agent(
            open(),
            "editor.ui_designer_set_instance_slot",
            json!({ "node": "#C1", "slot": "badge", "content": [crate::ui_designer::node_json(&preset("text").expect("preset"))] }),
        );
        assert_eq!(out["nodes"], 1);
        assert_eq!(
            write_ui_ron(twin.doc_mut().expect("doc").file()).expect("write"),
            ours,
            "the panel and the agent write the same bytes"
        );

        // A palette chip dropped on the slot row appends.
        panel.frame(Vec::new());
        let row = panel.rect("slot-row:badge").center();
        drag_chip(&mut panel, "button", row);
        let content = panel.doc().file().root.children[0]
            .slots
            .get("badge")
            .cloned()
            .expect("filled");
        assert_eq!(content.len(), 2, "{content:?}");
        assert_eq!(content[1], preset("button").expect("preset"));
        assert_eq!(panel.doc().undo_len(), undo + 2);

        // Clear: the component's default shows again, and the file is as it was.
        panel.click(panel.rect("slot-clear:badge").center());
        assert!(panel.doc().file().root.children[0].slots.is_empty());
        assert_eq!(write_ui_ron(panel.doc().file()).expect("write"), original);
        let (mut twin, _) = agent(
            twin,
            "editor.ui_designer_set_instance_slot",
            json!({ "node": "#C1", "slot": "badge", "content": null }),
        );
        assert_eq!(
            write_ui_ron(twin.doc_mut().expect("doc").file()).expect("write"),
            original
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-7716: `content: []` stores an empty slot (the component's default is hidden). The row
    /// says so, and Clear is enabled for it like for any other content: one undo step, the file as
    /// it was, the same bytes as the agent's call without `content`.
    #[test]
    fn an_empty_slot_is_labelled_and_clears() {
        assert_eq!(slot_summary(None), "component default");
        assert_eq!(slot_summary(Some(&[])), "empty (hides the default)");
        let dir = regia_test_utils::unique_temp_dir("ui-designer-panel-empty-slot");
        let ui_dir = dir.join("assets/ui");
        std::fs::create_dir_all(&ui_dir).expect("mkdir");
        std::fs::write(
            ui_dir.join("card.ui.ron"),
            r#"(version: 11, component: (slots: ["badge"]), root: (name: "Card", children: [(name: "Badge", slot: "badge", children: [(name: "DefaultBadge")])]))"#,
        )
        .expect("write component");
        let hud = ui_dir.join("hud.ui.ron");
        std::fs::write(
            &hud,
            r#"(version: 11, root: (name: "Hud", children: [(use: "ui/card.ui.ron", name: "C1")]))"#,
        )
        .expect("write hud");
        let open = || {
            let mut designer = UiDesigner::default();
            designer.open(&hud).expect("open");
            designer
        };
        let (designer, out) = agent(
            open(),
            "editor.ui_designer_set_instance_slot",
            json!({ "node": "#C1", "slot": "badge", "content": [] }),
        );
        assert_eq!(out["nodes"], 0);
        let mut panel = Panel {
            ctx: egui::Context::default(),
            designer,
            state: UiDesignerPanelState::default(),
            runtime: Default::default(),
        };
        assert_eq!(
            panel.doc().file().root.children[0]
                .slots
                .get("badge")
                .map(Vec::len),
            Some(0),
            "the agent stored an empty slot"
        );
        panel.doc().select(Some(vec![0])).expect("select");
        panel.frame(Vec::new());
        let undo = panel.doc().undo_len();
        panel.click(panel.rect("slot-clear:badge").center());
        assert!(panel.designer.error.is_none(), "{:?}", panel.designer.error);
        assert!(
            panel.doc().file().root.children[0].slots.is_empty(),
            "Clear removes the empty entry"
        );
        assert_eq!(panel.doc().undo_len(), undo + 1, "one undo step");
        let (twin, _) = agent(
            open(),
            "editor.ui_designer_set_instance_slot",
            json!({ "node": "#C1", "slot": "badge", "content": [] }),
        );
        let (mut twin, _) = agent(
            twin,
            "editor.ui_designer_set_instance_slot",
            json!({ "node": "#C1", "slot": "badge" }),
        );
        assert_eq!(
            write_ui_ron(twin.doc_mut().expect("doc").file()).expect("write"),
            write_ui_ron(panel.doc().file()).expect("write"),
            "the panel and the agent's call without content write the same bytes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn w12_aspect_row_is_keyboard_operable() {
        let ctx = egui::Context::default();
        let node = UiNodeDef::default();
        let mut output = Vec::new();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            aspect_ratio_row(ui, &node.style, &mut output);
            let id = ui
                .ctx()
                .data(|d| d.get_temp::<egui::Id>(egui::Id::new("ui-aspect-lock")))
                .expect("valid test fixture");
            let _ = id;
        })
        .textures_delta
        .clear();
        let target = ctx
            .data(|d| d.get_temp::<egui::Id>(egui::Id::new("ui-aspect-lock")))
            .expect("valid test fixture");
        tab_to(&ctx, target, |ui| {
            aspect_ratio_row(ui, &node.style, &mut output);
        });
        let mut passes = 0;
        ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Space,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| {
                passes += 1;
                if ui.ctx().current_pass_index() == 0 {
                    ui.ctx().request_discard("test: repeated layout pass");
                }
                aspect_ratio_row(ui, &node.style, &mut output);
            },
        )
        .textures_delta
        .clear();
        assert_eq!(passes, 2);
        assert_eq!(output.len(), 1, "one edit across both passes");
        assert!(
            output
                .iter()
                .any(|(key, value)| *key == "style.aspect_ratio" && value == &json!(1.0))
        );
    }

    #[test]
    fn w12_effect_add_is_keyboard_operable() {
        let ctx = egui::Context::default();
        let node = UiNodeDef {
            text: Some(regia_ui::authoring::TextDef::literal("Label")),
            ..Default::default()
        };
        let mut output = Vec::new();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            text_effect_rows(ui, &node, &mut output);
            let id = ui
                .ctx()
                .data(|d| d.get_temp::<egui::Id>(egui::Id::new("ui-effect-add-outline")))
                .expect("valid test fixture");
            let _ = id;
        })
        .textures_delta
        .clear();
        let target = ctx
            .data(|d| d.get_temp::<egui::Id>(egui::Id::new("ui-effect-add-outline")))
            .expect("valid test fixture");
        tab_to(&ctx, target, |ui| {
            text_effect_rows(ui, &node, &mut output);
        });
        ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Space,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| {
                text_effect_rows(ui, &node, &mut output);
            },
        )
        .textures_delta
        .clear();
        assert!(output.iter().any(
            |(key, value)| *key == "text.style.effects" && value.as_array().is_some_and(|a| a.len() == 1)
        ));
    }
    #[test]
    fn w12_preview_scale_is_keyboard_operable() {
        let ctx = egui::Context::default();
        let mut settings = PreviewSettings::default();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            preview_scale_rows(ui, &mut settings);
            let id = ui
                .ctx()
                .data(|d| d.get_temp::<egui::Id>(egui::Id::new("ui-manual-scale")))
                .expect("valid test fixture");
            let _ = id;
        })
        .textures_delta
        .clear();
        let target = ctx
            .data(|d| d.get_temp::<egui::Id>(egui::Id::new("ui-manual-scale")))
            .expect("valid test fixture");
        tab_to(&ctx, target, |ui| {
            preview_scale_rows(ui, &mut settings);
        });
        ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Space,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| preview_scale_rows(ui, &mut settings),
        )
        .textures_delta
        .clear();
        assert_eq!(settings.scale, Some(1.0));
    }

    #[test]
    fn w13_subtree_pivot_and_back_are_keyboard_operable() {
        let ctx = egui::Context::default();
        let node = UiNodeDef::default();
        let mut output = Vec::new();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            let _ = layout_rows(ui, &node);
        })
        .textures_delta
        .clear();
        let subtree = ctx
            .data(|d| d.get_temp::<egui::Id>(egui::Id::new("ui-designer-subtree")))
            .expect("valid test fixture");
        let pivot_x = ctx
            .data(|d| d.get_temp::<egui::Id>(egui::Id::new("ui-designer-pivot-x")))
            .expect("valid test fixture");
        tab_to(&ctx, subtree, |ui| {
            output.clear();
            let _ = layout_rows(ui, &node);
        });
        ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Space,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| {
                output.clear();
                output.extend(layout_rows(ui, &node));
            },
        )
        .textures_delta
        .clear();
        assert!(
            output
                .iter()
                .any(|(key, value)| *key == "disabled_subtree" && value == &json!(true)),
            "Space on Disable subtree writes the field: {output:?}"
        );
        tab_to(&ctx, pivot_x, |ui| {
            let _ = layout_rows(ui, &node);
        });
        assert_eq!(
            ctx.memory(|m| m.focused()),
            Some(pivot_x),
            "Pivot X is in the Tab order"
        );

        let mut designer = UiDesigner::default();
        designer.new_document();
        let doc = designer.doc_mut().expect("valid test fixture");
        let back_ctx = egui::Context::default();
        back_ctx
            .run_ui(egui::RawInput::default(), |ui| screen_back_rows(ui, doc))
            .textures_delta
            .clear();
        let pop = back_ctx
            .data(|d| d.get_temp::<egui::Id>(egui::Id::new("ui-designer-back-pop")))
            .expect("valid test fixture");
        tab_to(&back_ctx, pop, |ui| screen_back_rows(ui, doc));
        let depth = doc.undo_len();
        back_ctx
            .run_ui(
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key: egui::Key::Space,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    ..Default::default()
                },
                |ui| screen_back_rows(ui, doc),
            )
            .textures_delta
            .clear();
        assert_eq!(doc.file().back, Some(ScreenBackDef::Pop));
        assert_eq!(doc.undo_len(), depth + 1, "one undo step for the back choice");
    }

    #[test]
    fn w12_anchor_grid_is_keyboard_operable_and_one_undo_step() {
        let ctx = egui::Context::default();
        let mut designer = UiDesigner::default();
        designer.new_document();
        let doc = designer.doc_mut().expect("valid test fixture");
        doc.apply(DesignOp::Add {
            parent: vec![],
            index: None,
            node: Box::new(preset("button").expect("valid test fixture")),
        })
        .expect("valid test fixture");
        let depth = doc.undo_len();
        let mut state = UiDesignerPanelState {
            fresh_input: true,
            snap_anchor_position: true,
            ..Default::default()
        };
        let mut error = None;
        ctx.run_ui(egui::RawInput::default(), |ui| {
            anchor_rows(ui, doc, &mut state, &[0], &mut error);
            let id = ui
                .ctx()
                .data(|d| d.get_temp::<egui::Id>(egui::Id::new("ui-anchor-top-right")))
                .expect("valid test fixture");
            let _ = id;
        })
        .textures_delta
        .clear();
        let target = ctx
            .data(|d| d.get_temp::<egui::Id>(egui::Id::new("ui-anchor-top-right")))
            .expect("valid test fixture");
        tab_to(&ctx, target, |ui| {
            anchor_rows(ui, doc, &mut state, &[0], &mut error);
        });
        ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Space,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| anchor_rows(ui, doc, &mut state, &[0], &mut error),
        )
        .textures_delta
        .clear();
        assert!(error.is_none(), "{error:?}");
        assert_eq!(doc.undo_len(), depth + 1);
        assert_eq!(
            doc.file().root.children[0].style.inset.right,
            regia_ui::Dimension::Points(0.0)
        );
    }
    fn tab_to(ctx: &egui::Context, target: egui::Id, mut draw: impl FnMut(&mut egui::Ui)) {
        for _ in 0..96 {
            if ctx.memory(|m| m.focused()) == Some(target) {
                return;
            }
            ctx.run_ui(
                egui::RawInput {
                    events: [true, false]
                        .into_iter()
                        .map(|pressed| egui::Event::Key {
                            key: egui::Key::Tab,
                            physical_key: None,
                            pressed,
                            repeat: false,
                            modifiers: egui::Modifiers::NONE,
                        })
                        .collect(),
                    ..Default::default()
                },
                |ui| draw(ui),
            )
            .textures_delta
            .clear();
        }
        panic!("control is not reachable with Tab");
    }

    const PARITY: &str = include_str!("../../regia_ui/tests/fixtures/ui.v11-parity.example.ui.ron");

    impl Panel {
        /// Focuses the widget recorded under `key` the way Tab would (no pointer).
        fn focus(&mut self, key: &str) {
            let id = self
                .ctx
                .data_mut(|d| d.get_temp::<egui::Id>(egui::Id::new(("ui-designer-test-id", key))))
                .unwrap_or_else(|| panic!("no id for {key}"));
            self.ctx.memory_mut(|m| m.request_focus(id));
            self.frame(Vec::new());
        }

        fn select_named(&mut self, name: &str) {
            let doc = self.doc();
            let path = crate::ui_designer::resolve_node(&doc.file().root, &format!("#{name}")).expect("node");
            doc.select(Some(path)).expect("select");
            self.frame(Vec::new());
            self.frame(Vec::new());
        }
    }

    /// ADR-7827 accessibility: the Navigation section's neighbor rows take a node name from the
    /// keyboard alone (focus, type, Enter: one undo step), and Pick names a neighbor by a click on
    /// the preview node.
    #[test]
    fn navigation_rows_set_neighbors_from_the_keyboard_and_by_picking() {
        let mut panel = Panel::with_text(PARITY);
        panel.select_named("reset");
        let undo = panel.doc().undo_len();
        panel.focus("nav down");
        panel.frame(vec![egui::Event::Text("quit".to_owned())]);
        panel.key(egui::Key::Enter, egui::Modifiers::NONE);
        panel.frame(Vec::new());
        let reset = |panel: &mut Panel| panel.doc().file().root.children[2].children[1].nav.clone();
        assert_eq!(reset(&mut panel).down.as_deref(), Some("quit"));
        assert_eq!(
            reset(&mut panel).left.as_deref(),
            Some("apply"),
            "the other neighbor stays"
        );
        assert_eq!(panel.doc().undo_len(), undo + 1);

        // Pick: arm the up row, click the title node in the preview.
        panel.click(panel.rect("nav-pick:up").center());
        assert_eq!(panel.state.nav_pick, Some("up"));
        panel.click(panel.rect("preview:/0").center());
        assert_eq!(reset(&mut panel).up.as_deref(), Some("title"));
        assert_eq!(panel.state.nav_pick, None, "one pick per arm");
        assert_eq!(
            panel.doc().selection,
            Some(vec![2, 1]),
            "a pick does not change the selection"
        );
        // Clearing a row from the keyboard removes the neighbor.
        panel.focus("nav up");
        panel.key(egui::Key::A, egui::Modifiers::COMMAND);
        panel.key(egui::Key::Backspace, egui::Modifiers::NONE);
        panel.key(egui::Key::Enter, egui::Modifiers::NONE);
        panel.frame(Vec::new());
        assert_eq!(reset(&mut panel).up, None);

        // Show navigation draws over the preview and is panel state only.
        let before = panel.doc().text().expect("text");
        panel.click(panel.rect("view:nav").center());
        assert!(panel.state.show_nav);
        panel.frame(Vec::new());
        assert_eq!(panel.doc().text().expect("text"), before);
    }

    /// ADR-7827 accessibility: the Markup box toggles from the keyboard (focus, Space) and the
    /// preview row shows the parsed text.
    #[test]
    fn the_markup_box_toggles_from_the_keyboard() {
        let mut panel = Panel::with_text(PARITY);
        panel.select_named("general");
        panel.focus("markup");
        panel.key(egui::Key::Space, egui::Modifiers::NONE);
        panel.frame(Vec::new());
        let markup = |panel: &mut Panel| {
            panel.doc().file().root.children[1].children[1].children[0]
                .text
                .as_ref()
                .is_some_and(|t| t.markup)
        };
        assert!(markup(&mut panel), "Space on the focused box turns markup on");
        panel.frame(Vec::new());
        let _ = panel.rect("markup-preview");
        panel.focus("markup");
        panel.key(egui::Key::Space, egui::Modifiers::NONE);
        panel.frame(Vec::new());
        assert!(!markup(&mut panel), "and off again");
        assert!(
            !panel.doc().text().expect("text").contains("markup: false"),
            "off is not written"
        );
    }
}
