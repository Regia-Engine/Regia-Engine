// SPDX-License-Identifier: MPL-2.0
//! The command palette (ADR-273 E02, ADR-1842): one searchable list of editor actions, opened with
//! Ctrl+Shift+P (Cmd+Shift+P on macOS).
//!
//! [`COMMANDS`] is the registry; [`CommandPalette`] (a world resource) holds the open flag, the
//! query, the highlighted row and the recently used commands. Ranking is the fuzzy matcher of
//! `regia_ui::fuzzy` over `category: title`, with a small bonus for recent commands so the ones
//! you use float up. [`run`] executes a command against the world, which is what the egui
//! window and the `editor.command` agent method both call, so the two cannot drift apart.
//!
//! Rebinding (ADR-2045): [`CommandKeymap`] adds user shortcuts to any command. In the palette,
//! F2 on the highlighted row records the next key chord as its shortcut (Escape while recording
//! clears it); `editor.command { bind }` does the same from an agent. User shortcuts run the
//! command through [`run`]; the built-in undo and redo keys keep working beside them. The keymap
//! is saved per user by `crate::editor_keymap` (ADR-2321), which also finds conflicts and imports
//! and exports keymaps.
//!
//! The agent surface is `editor.command` (`book/agents/editor-command.md`).

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_ui::fuzzy::fuzzy_match;
use serde_json::{Value, json};

use crate::selection::Selection;
use crate::viewport::{EditorCamera, ViewportSettings};

/// Recent commands remembered.
const RECENT_LEN: usize = 5;
/// Score bonus for the most recent command; older ones get less.
const RECENT_BONUS: i32 = 40;
/// Rows the window shows.
const WINDOW_ROWS: usize = 12;

/// One palette entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandDef {
    /// Stable id (`group.action`), the `editor.command` `run` argument.
    pub id: &'static str,
    /// Group shown before the title.
    pub category: &'static str,
    /// What the command does.
    pub title: &'static str,
    /// The keyboard shortcut, for display (empty for none).
    pub shortcut: &'static str,
}

/// The palette commands written by hand. [`COMMANDS`] is this list plus one `window.open_<panel id>`
/// command per built-in panel, generated from the panel list so a new panel cannot be missed.
const FIXED_COMMANDS: &[CommandDef] = &[
    // The palette's first row with an empty query (P-019, ADR-7766).
    CommandDef {
        id: "search.everything",
        category: "Search",
        title: "Search everything…",
        shortcut: "Ctrl+T",
    },
    CommandDef {
        id: "edit.undo",
        category: "Edit",
        title: "Undo",
        shortcut: "Ctrl+Z",
    },
    CommandDef {
        id: "edit.redo",
        category: "Edit",
        title: "Redo",
        shortcut: "Ctrl+Shift+Z",
    },
    CommandDef {
        id: "view.frame_selection",
        category: "View",
        title: "Frame selection",
        shortcut: "F",
    },
    CommandDef {
        id: "view.toggle_grid",
        category: "View",
        title: "Toggle grid",
        shortcut: "",
    },
    CommandDef {
        id: "view.toggle_labels",
        category: "View",
        title: "Toggle selection labels",
        shortcut: "",
    },
    CommandDef {
        id: "view.toggle_realtime",
        category: "View",
        title: "Toggle realtime viewport",
        shortcut: "",
    },
    CommandDef {
        id: "view.toggle_audio_gizmos",
        category: "View",
        title: "Toggle audio gizmos",
        shortcut: "",
    },
    CommandDef {
        id: "gizmo.translate",
        category: "Gizmo",
        title: "Translate",
        shortcut: "W",
    },
    CommandDef {
        id: "gizmo.rotate",
        category: "Gizmo",
        title: "Rotate",
        shortcut: "E",
    },
    CommandDef {
        id: "gizmo.scale",
        category: "Gizmo",
        title: "Scale",
        shortcut: "R",
    },
    CommandDef {
        id: "window.preferences",
        category: "Window",
        title: "Preferences",
        shortcut: "",
    },
    CommandDef {
        id: "window.project_settings",
        category: "Window",
        title: "Project settings",
        shortcut: "",
    },
    CommandDef {
        id: "layout.save",
        category: "Layout",
        title: "Save layout to project",
        shortcut: "",
    },
    CommandDef {
        id: "layout.save_default",
        category: "Layout",
        title: "Save layout as my default",
        shortcut: "",
    },
    CommandDef {
        id: "layout.reset",
        category: "Layout",
        title: "Reset layout",
        shortcut: "",
    },
    CommandDef {
        id: "window.world_settings",
        category: "Window",
        title: "World settings",
        shortcut: "",
    },
    CommandDef {
        id: "gizmo.vertex_snap",
        category: "Gizmo",
        title: "Vertex snap (hold while translating; the palette toggles it)",
        shortcut: "V",
    },
    CommandDef {
        id: "prefab.create_variant",
        category: "Prefab",
        title: "Create variant of the selected prefab instance…",
        shortcut: "",
    },
    CommandDef {
        id: "selection.drop_to_floor",
        category: "Selection",
        title: "Drop to floor",
        shortcut: "End",
    },
    CommandDef {
        id: "placement.toggle_align_normal",
        category: "Selection",
        title: "Toggle align to surface normal",
        shortcut: "",
    },
    CommandDef {
        id: "collider.toggle_handles",
        category: "Selection",
        title: "Toggle collider handles",
        shortcut: "C",
    },
    CommandDef {
        id: "camera.fly_forward",
        category: "Camera",
        title: "Fly forward (hold with the right mouse button)",
        shortcut: "W",
    },
    CommandDef {
        id: "camera.fly_back",
        category: "Camera",
        title: "Fly back (hold with the right mouse button)",
        shortcut: "S",
    },
    CommandDef {
        id: "camera.fly_left",
        category: "Camera",
        title: "Fly left (hold with the right mouse button)",
        shortcut: "A",
    },
    CommandDef {
        id: "camera.fly_right",
        category: "Camera",
        title: "Fly right (hold with the right mouse button)",
        shortcut: "D",
    },
    CommandDef {
        id: "camera.fly_up",
        category: "Camera",
        title: "Fly up (hold with the right mouse button)",
        shortcut: "E",
    },
    CommandDef {
        id: "camera.fly_down",
        category: "Camera",
        title: "Fly down (hold with the right mouse button)",
        shortcut: "Q",
    },
    CommandDef {
        id: "outliner.delete",
        category: "Outliner",
        title: "Delete the selection",
        shortcut: "Delete",
    },
    CommandDef {
        id: "outliner.duplicate",
        category: "Outliner",
        title: "Duplicate the selection",
        shortcut: "Ctrl+D",
    },
    CommandDef {
        id: "outliner.group",
        category: "Outliner",
        title: "Group the selection",
        shortcut: "Ctrl+G",
    },
    CommandDef {
        id: "file.new_level",
        category: "File",
        title: "New level (refused while the level has unsaved changes or a prefab is open)",
        shortcut: "",
    },
    CommandDef {
        id: "file.new_level_discard",
        category: "File",
        title: "New level, discarding unsaved changes",
        shortcut: "",
    },
    CommandDef {
        id: "recovery.restore",
        category: "File",
        title: "Restore the autosave the crash-recovery window offers",
        shortcut: "",
    },
    CommandDef {
        id: "recovery.discard",
        category: "File",
        title: "Discard the autosave the crash-recovery window offers",
        shortcut: "",
    },
    CommandDef {
        id: "viewport.toggle_spacemouse_object",
        category: "View",
        title: "Toggle the SpaceMouse Object tool (the cap moves the selection)",
        shortcut: "",
    },
    CommandDef {
        id: "material.undo",
        category: "Material",
        title: "Undo in the Material panel",
        shortcut: "",
    },
    CommandDef {
        id: "material.redo",
        category: "Material",
        title: "Redo in the Material panel",
        shortcut: "",
    },
    CommandDef {
        id: "material.reload",
        category: "Material",
        title: "Reload the Material panel's material from disk (drops unsaved edits)",
        shortcut: "",
    },
    CommandDef {
        id: "terrain.retry_level_terrain",
        category: "Terrain",
        title: "Retry the level terrain load after an error",
        shortcut: "",
    },
    CommandDef {
        id: "lightmap.discard_checkpoint",
        category: "Lightmaps",
        title: "Discard the bake checkpoint (the next bake starts over)",
        shortcut: "",
    },
    CommandDef {
        id: "view.toggle_frame_debugger",
        category: "View",
        title: "Toggle the Frame Debugger window",
        shortcut: "",
    },
    CommandDef {
        id: "view.toggle_profiler",
        category: "View",
        title: "Toggle the Profiler window",
        shortcut: "",
    },
    CommandDef {
        id: "view.toggle_diagnostics",
        category: "View",
        title: "Toggle the Diagnostics window",
        shortcut: "",
    },
    CommandDef {
        id: "create.cuboid",
        category: "Create",
        title: "Cuboid shape",
        shortcut: "",
    },
    CommandDef {
        id: "create.sphere",
        category: "Create",
        title: "Sphere shape",
        shortcut: "",
    },
    CommandDef {
        id: "create.capsule",
        category: "Create",
        title: "Capsule shape",
        shortcut: "",
    },
    CommandDef {
        id: "create.cylinder",
        category: "Create",
        title: "Cylinder shape",
        shortcut: "",
    },
    CommandDef {
        id: "create.cone",
        category: "Create",
        title: "Cone shape",
        shortcut: "",
    },
    CommandDef {
        id: "create.conical_frustum",
        category: "Create",
        title: "Conical frustum shape",
        shortcut: "",
    },
    CommandDef {
        id: "create.plane",
        category: "Create",
        title: "Plane shape",
        shortcut: "",
    },
    CommandDef {
        id: "create.torus",
        category: "Create",
        title: "Torus shape",
        shortcut: "",
    },
    CommandDef {
        id: "create.tetrahedron",
        category: "Create",
        title: "Tetrahedron shape",
        shortcut: "",
    },
    CommandDef {
        id: "create.ramp",
        category: "Create",
        title: "Ramp shape",
        shortcut: "",
    },
    CommandDef {
        id: "create.prism",
        category: "Create",
        title: "Prism shape",
        shortcut: "",
    }, // ADR-7733: the interface zoom, saved to `appearance.ui_scale` (egui's own keyboard zoom is
    // off: its Ctrl+0 reset clashed with camera bookmark 0).
    CommandDef {
        id: "view.zoom_in",
        category: "View",
        title: "Zoom in (interface)",
        shortcut: "Ctrl+Plus",
    },
    CommandDef {
        id: "view.zoom_out",
        category: "View",
        title: "Zoom out (interface)",
        shortcut: "Ctrl+Minus",
    },
    CommandDef {
        id: "view.reset_zoom",
        category: "View",
        title: "Reset zoom (interface)",
        shortcut: "Ctrl+Alt+0",
    },
    // Gizmo tools, space, pivot offset and steps (ADR-7756).
    CommandDef {
        id: "gizmo.select",
        category: "Gizmo",
        title: "Select tool (no gizmo)",
        shortcut: "Q",
    },
    CommandDef {
        id: "gizmo.toggle_space",
        category: "Gizmo",
        title: "Next reference space (world, local, parent, view)",
        shortcut: "Ctrl+`",
    },
    CommandDef {
        id: "gizmo.pivot_offset",
        category: "Gizmo",
        title: "Move the pivot (hold while dragging the rotate or scale gizmo; the palette clears the offset)",
        shortcut: "P",
    },
    CommandDef {
        id: "gizmo.rotate_step",
        category: "Gizmo",
        title: "Rotate the selection by the angle step",
        shortcut: "U",
    },
    CommandDef {
        id: "gizmo.rotate_step_back",
        category: "Gizmo",
        title: "Rotate the selection back by the angle step",
        shortcut: "Shift+U",
    },
    CommandDef {
        id: "placement.toggle_surface_snap",
        category: "Selection",
        title: "Toggle surface snap while dragging",
        shortcut: "",
    },
    CommandDef {
        id: "placement.toggle_surface_align",
        category: "Selection",
        title: "Toggle align to surface while snapping",
        shortcut: "",
    },
    CommandDef {
        id: "gizmo.toggle_snap",
        category: "Gizmo",
        title: "Toggle snapping to the grid, angle and scale steps",
        shortcut: "",
    },
    // Maximize, full screen and workspaces (`crate::workspaces`, ADR-7753).
    CommandDef {
        id: "view.maximize_panel",
        category: "View",
        title: "Maximize or restore the panel",
        shortcut: "Ctrl+Space",
    },
    CommandDef {
        id: "view.fullscreen_viewport",
        category: "View",
        title: "Full-screen viewport",
        shortcut: "F11",
    },
    CommandDef {
        id: "workspace.level",
        category: "Workspace",
        title: "Level",
        shortcut: "",
    },
    CommandDef {
        id: "workspace.animation",
        category: "Workspace",
        title: "Animation",
        shortcut: "",
    },
    CommandDef {
        id: "workspace.materials",
        category: "Workspace",
        title: "Materials",
        shortcut: "",
    },
    CommandDef {
        id: "workspace.ui",
        category: "Workspace",
        title: "UI",
        shortcut: "",
    },
    CommandDef {
        id: "workspace.world",
        category: "Workspace",
        title: "World",
        shortcut: "",
    },
    CommandDef {
        id: "workspace.cinematics",
        category: "Workspace",
        title: "Cinematics",
        shortcut: "",
    },
    CommandDef {
        id: "workspace.debug",
        category: "Workspace",
        title: "Debug",
        shortcut: "",
    },
    CommandDef {
        id: "view.toggle_motion_database",
        category: "View",
        title: "Toggle the Motion database editor window",
        shortcut: "",
    },
    // Drawers (ADR-7754; `window.open_problems` is one of the generated panel commands).
    CommandDef {
        id: "view.content_drawer",
        category: "View",
        title: "Toggle the content drawer",
        shortcut: "Ctrl+Shift+Space",
    },
    CommandDef {
        id: "view.log_drawer",
        category: "View",
        title: "Toggle the log drawer and console",
        shortcut: "",
    },
    CommandDef {
        id: "view.dock_drawer",
        category: "View",
        title: "Dock the open drawer as a tab",
        shortcut: "",
    },
    // Viewport camera (ADR-7755, `crate::camera_nav::COMMAND_IDS`).
    CommandDef {
        id: "view.camera_to_selection",
        category: "View",
        title: "Move camera to selection",
        shortcut: "",
    },
    CommandDef {
        id: "view.selection_to_camera",
        category: "View",
        title: "Move selection to camera",
        shortcut: "",
    },
    CommandDef {
        id: "view.toggle_orbit_selection",
        category: "View",
        title: "Toggle orbit around selection",
        shortcut: "",
    },
    CommandDef {
        id: "view.toggle_zoom_to_cursor",
        category: "View",
        title: "Toggle zoom toward cursor",
        shortcut: "",
    },
    CommandDef {
        id: "camera.speed_up",
        category: "View",
        title: "Camera speed: next step",
        shortcut: "",
    },
    CommandDef {
        id: "camera.speed_down",
        category: "View",
        title: "Camera speed: previous step",
        shortcut: "",
    },
    CommandDef {
        id: "view.cycle_layout",
        category: "View",
        title: "Next viewport layout (1, 2, 3, 4 panes)",
        shortcut: "",
    },
    // Favorites and the selection history (P-030, P-033, ADR-7766).
    CommandDef {
        id: "view.favorites",
        category: "View",
        title: "Favorites…",
        shortcut: "Ctrl+Shift+F",
    },
    CommandDef {
        id: "selection.back",
        category: "Selection",
        title: "Back to the previous selection",
        shortcut: "Alt+ArrowLeft",
    },
    CommandDef {
        id: "selection.forward",
        category: "Selection",
        title: "Forward to the next selection",
        shortcut: "Alt+ArrowRight",
    },
    CommandDef {
        id: "selection.history",
        category: "Selection",
        title: "Selection history…",
        shortcut: "",
    },
    // Contextual help (ADR-7767): the book page of the focused panel, or the agent page section of
    // the hovered Inspector field.
    CommandDef {
        id: "help.context",
        category: "Help",
        title: "Help for the focused panel",
        shortcut: "F1",
    },
    // Pilot, look through and screenshots (ADR-7768).
    CommandDef {
        id: "view.pilot",
        category: "View",
        title: "Pilot the selection (again to eject)",
        shortcut: "Ctrl+Shift+L",
    },
    CommandDef {
        id: "view.eject",
        category: "View",
        title: "Eject from the piloted entity",
        shortcut: "",
    },
    CommandDef {
        id: "view.look_through_next",
        category: "View",
        title: "Look through the next scene camera",
        shortcut: "",
    },
    CommandDef {
        id: "view.editor_camera",
        category: "View",
        title: "Back to the editor camera",
        shortcut: "",
    },
    CommandDef {
        id: "file.screenshot",
        category: "File",
        title: "Screenshot…",
        shortcut: "",
    },
    // Help ▸ Samples… (w13): the keyboard path to the samples browser.
    CommandDef {
        id: "help.samples",
        category: "Help",
        title: "Samples…",
        shortcut: "",
    },
    // Viewport selection and placement tools (P-064..P-074, ADR-7769).
    CommandDef {
        id: "selection.similar_kind",
        category: "Selection",
        title: "Select all of the same kind",
        shortcut: "",
    },
    CommandDef {
        id: "selection.similar_components",
        category: "Selection",
        title: "Select all with the same components",
        shortcut: "",
    },
    CommandDef {
        id: "selection.similar_asset",
        category: "Selection",
        title: "Select all using the same asset",
        shortcut: "",
    },
    CommandDef {
        id: "selection.similar_layer",
        category: "Selection",
        title: "Select all in the same layer",
        shortcut: "",
    },
    CommandDef {
        id: "selection.lasso",
        category: "Selection",
        title: "Lasso select (hold while dragging; the palette toggles lasso drags)",
        shortcut: "L",
    },
    CommandDef {
        id: "selection.pick_stack",
        category: "Selection",
        title: "List the objects under the cursor (also Alt+click)",
        shortcut: "",
    },
    CommandDef {
        id: "selection.toggle_root",
        category: "Selection",
        title: "Toggle selection root (clicks on children select it)",
        shortcut: "",
    },
    CommandDef {
        id: "viewport.measure",
        category: "Viewport",
        title: "Measure distance and angle",
        shortcut: "M",
    }, // The active pane's view mode and the editor-only hide (ADR-7770).
    CommandDef {
        id: "view.mode_1",
        category: "View",
        title: "View mode: Lit",
        shortcut: "Alt+1",
    },
    CommandDef {
        id: "view.mode_2",
        category: "View",
        title: "View mode: Unlit",
        shortcut: "Alt+2",
    },
    CommandDef {
        id: "view.mode_3",
        category: "View",
        title: "View mode: Wireframe",
        shortcut: "Alt+3",
    },
    CommandDef {
        id: "view.mode_4",
        category: "View",
        title: "View mode: Lit + wireframe",
        shortcut: "Alt+4",
    },
    CommandDef {
        id: "view.mode_5",
        category: "View",
        title: "View mode: Lighting only",
        shortcut: "Alt+5",
    },
    CommandDef {
        id: "view.mode_6",
        category: "View",
        title: "View mode: Light complexity",
        shortcut: "Alt+6",
    },
    CommandDef {
        id: "view.mode_7",
        category: "View",
        title: "View mode: Overdraw",
        shortcut: "Alt+7",
    },
    CommandDef {
        id: "view.mode_8",
        category: "View",
        title: "View mode: Base color buffer",
        shortcut: "Alt+8",
    },
    CommandDef {
        id: "view.hide_selection",
        category: "View",
        title: "Hide the selection in the editor",
        shortcut: "H",
    },
    CommandDef {
        id: "view.unhide_all",
        category: "View",
        title: "Show everything hidden in the editor",
        shortcut: "Alt+H",
    },
    CommandDef {
        id: "view.isolate",
        category: "View",
        title: "Isolate the selection (again: show the rest)",
        shortcut: "Shift+H",
    },
    CommandDef {
        id: "view.toggle_rendering_debug",
        category: "View",
        title: "Toggle the Rendering Debug window",
        shortcut: "",
    },
    CommandDef {
        id: "window.asset_jobs",
        category: "Window",
        title: "Toggle the Asset Jobs window",
        shortcut: "",
    },
    CommandDef {
        id: "prefab.overrides",
        category: "Prefab",
        title: "Overrides…",
        shortcut: "",
    },
    CommandDef {
        id: "prefab.edit_in_place",
        category: "Prefab",
        title: "Edit in place",
        shortcut: "",
    },
    CommandDef {
        id: "view.toggle_revision_control",
        category: "View",
        title: "Toggle the Revision Control window",
        shortcut: "",
    },
    CommandDef {
        id: "review.add_note",
        category: "Review",
        title: "Add review note",
        shortcut: "N",
    },
    // Floated panels (`crate::os_windows`, ADR-7821): the keyboard path of the tab menu items.
    CommandDef {
        id: "view.float_panel",
        category: "View",
        title: "Float the focused panel in its own window",
        shortcut: "",
    },
    CommandDef {
        id: "view.dock_floated_panels",
        category: "View",
        title: "Dock every floated panel back",
        shortcut: "",
    },
    CommandDef {
        id: "window.collaboration",
        category: "Window",
        title: "Collaboration",
        shortcut: "",
    },
    CommandDef {
        id: "view.toggle_file_locks",
        category: "View",
        title: "Toggle the File Locks window",
        shortcut: "",
    },
    CommandDef {
        id: "vcs.unlock_all_mine",
        category: "Revision control",
        title: "Unlock all my file locks",
        shortcut: "",
    },
    // Panel instances (`crate::panel_instances`, P-003): the keyboard path of Window ▸ New ….
    CommandDef {
        id: "window.new_outliner",
        category: "Window",
        title: "New Outliner",
        shortcut: "",
    },
    CommandDef {
        id: "window.new_content",
        category: "Window",
        title: "New Content Browser",
        shortcut: "",
    },
    CommandDef {
        id: "window.new_inspector",
        category: "Window",
        title: "New Inspector",
        shortcut: "",
    },
    CommandDef {
        id: "workspace.terrain",
        category: "Workspace",
        title: "Terrain workspace",
        shortcut: "",
    },
    CommandDef {
        id: "workspace.tiles2d",
        category: "Workspace",
        title: "2D tiles workspace",
        shortcut: "",
    },
];

/// Commands whose built-in shortcut [`show`] runs itself (the other built-in shortcuts are read
/// by the panel that owns them); a user binding replaces it (ADR-7733).
pub const GLOBAL_SHORTCUT_COMMANDS: &[&str] = &["view.zoom_in", "view.zoom_out", "view.reset_zoom"];

/// One `window.open_<panel id>` command per built-in panel ([`crate::editor_layout::BUILT_IN_PANELS`],
/// which `Panel::id` keeps complete), titled `Open the <panel title> tab`. The strings are leaked once
/// (about sixty short ones) because [`CommandDef`] holds `&'static str`.
fn panel_commands() -> Vec<CommandDef> {
    crate::editor_layout::BUILT_IN_PANELS
        .iter()
        .map(|panel| CommandDef {
            id: Box::leak(format!("{OPEN_PANEL_PREFIX}{}", panel.id()).into_boxed_str()),
            category: "Window",
            title: Box::leak(format!("Open the {} tab", panel.title()).into_boxed_str()),
            shortcut: "",
        })
        .collect()
}

/// Every command the palette offers: the hand-written ones (`FIXED_COMMANDS`) with the generated panel commands after
/// `window.world_settings`.
pub static COMMANDS: std::sync::LazyLock<Vec<CommandDef>> = std::sync::LazyLock::new(|| {
    let mut all = Vec::with_capacity(FIXED_COMMANDS.len() + crate::editor_layout::BUILT_IN_PANELS.len());
    for def in FIXED_COMMANDS {
        all.push(*def);
        if def.id == "window.world_settings" {
            all.extend(panel_commands());
        }
    }
    all
});

/// The id prefix of the commands that bring a panel tab forward (`window.open_<panel id>`, the ids
/// of `editor.layout_apply`'s `open`): the keyboard route to every main panel, through the palette
/// (Ctrl+Shift+P) or a user shortcut (ADR-9810).
pub const OPEN_PANEL_PREFIX: &str = "window.open_";

/// Commands whose key is held, not pressed: the viewport reads them with [`hold_chord`]. A user
/// shortcut never runs them through [`run`]; it only moves the key.
pub const HOLD_COMMANDS: &[&str] = &[
    "gizmo.vertex_snap",
    "camera.fly_forward",
    "camera.fly_back",
    "camera.fly_left",
    "camera.fly_right",
    "camera.fly_up",
    "camera.fly_down",
    "gizmo.pivot_offset",
    "selection.lasso",
];

/// The chord of a hold command: the user's when bound, else the built-in shortcut.
#[must_use]
pub fn hold_chord(world: &World, id: &str) -> Option<KeyChord> {
    world
        .get_resource::<CommandKeymap>()
        .and_then(|k| k.bindings.get(id).copied())
        .or_else(|| KeyChord::parse(find(id)?.shortcut))
}

/// Whether `chord` is held: its key is down and every modifier it names is too (others may be held
/// as well, like Shift while flying).
#[must_use]
pub fn chord_held(input: &egui::InputState, chord: KeyChord) -> bool {
    input.key_down(chord.key)
        && (!chord.command || input.modifiers.command)
        && (!chord.shift || input.modifiers.shift)
        && (!chord.alt || input.modifiers.alt)
}

/// A key chord: modifiers plus one key, written `Ctrl+Shift+K` (`Cmd` is the macOS name of
/// `Ctrl`; `Alt` and `Shift` as usual).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyChord {
    /// Ctrl (Cmd on macOS).
    pub command: bool,
    /// Shift.
    pub shift: bool,
    /// Alt (Option on macOS).
    pub alt: bool,
    /// The key.
    pub key: egui::Key,
}

impl KeyChord {
    /// Parses `Ctrl+Shift+K`, `Alt+F5`, `F` (case-insensitive modifier names; the key is an egui
    /// key name such as `K`, `F5`, `Space`, `ArrowUp`). `None` for an unknown key, a repeated
    /// modifier or an empty chord.
    pub fn parse(text: &str) -> Option<Self> {
        let parts: Vec<&str> = text.split('+').map(str::trim).collect();
        let (key, mods) = parts.split_last()?;
        let mut chord = Self {
            command: false,
            shift: false,
            alt: false,
            key: egui::Key::from_name(key).or_else(|| egui::Key::from_name(&key.to_uppercase()))?,
        };
        for m in mods {
            let slot = match m.to_ascii_lowercase().as_str() {
                "ctrl" | "cmd" | "command" => &mut chord.command,
                "shift" => &mut chord.shift,
                "alt" | "option" => &mut chord.alt,
                _ => return None,
            };
            if *slot {
                return None;
            }
            *slot = true;
        }
        Some(chord)
    }

    /// The `Ctrl+Shift+K` form ([`Self::parse`] reads it back).
    pub fn label(&self) -> String {
        let mut out = String::new();
        for (on, name) in [
            (self.command, "Ctrl+"),
            (self.alt, "Alt+"),
            (self.shift, "Shift+"),
        ] {
            if on {
                out.push_str(name);
            }
        }
        out.push_str(self.key.name());
        out
    }

    /// The egui shortcut of the chord.
    pub fn shortcut(&self) -> egui::KeyboardShortcut {
        let mut m = egui::Modifiers::NONE;
        if self.command {
            m |= egui::Modifiers::COMMAND;
        }
        if self.shift {
            m |= egui::Modifiers::SHIFT;
        }
        if self.alt {
            m |= egui::Modifiers::ALT;
        }
        egui::KeyboardShortcut::new(m, self.key)
    }
}

/// User shortcuts per command id (ADR-2045), checked every editor frame by [`show`].
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct CommandKeymap {
    /// Command id → chord.
    pub bindings: std::collections::BTreeMap<&'static str, KeyChord>,
    /// The command whose next key chord the palette records (F2), if any.
    pub recording: Option<&'static str>,
    /// The scope of each user binding that is not global (ADR-7767); a missing id is global.
    pub scopes: std::collections::BTreeMap<&'static str, crate::editor_keymap::ShortcutScope>,
}

/// Why a binding was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindError {
    /// No command has the id.
    UnknownCommand,
    /// The chord is reserved: Ctrl+Shift+P opens the palette.
    Reserved,
    /// A safety key (stop play, save, undo, redo) bound in a non-global scope
    /// (`crate::editor_keymap::SAFETY_KEYS`; code `shortcut_safety_key`).
    SafetyKey,
}

impl CommandKeymap {
    /// Binds `chord` to `id` in the command's current scope (for a command without a user binding,
    /// its built-in shortcut's scope: a viewport command stays in the viewport), taking it off any
    /// other command of that scope first. Returns the command it was taken from, if any.
    pub fn bind(&mut self, id: &str, chord: KeyChord) -> Result<Option<&'static str>, BindError> {
        let scope = if self.bindings.contains_key(id) {
            self.scope_of(id)
        } else {
            crate::editor_keymap::builtin_scope(id)
        };
        self.bind_in(id, chord, scope)
    }

    /// Binds `chord` to `id` in `scope` (ADR-7767), taking it off any other command bound to it in
    /// the same scope (a binding in another scope keeps it). A safety key is refused in a
    /// non-global scope. Returns the command the chord was taken from, if any.
    pub fn bind_in(
        &mut self,
        id: &str,
        chord: KeyChord,
        scope: crate::editor_keymap::ShortcutScope,
    ) -> Result<Option<&'static str>, BindError> {
        use crate::editor_keymap::ShortcutScope;
        let def = find(id).ok_or(BindError::UnknownCommand)?;
        if chord.command && chord.shift && !chord.alt && chord.key == egui::Key::P {
            return Err(BindError::Reserved);
        }
        if scope != ShortcutScope::Global && crate::editor_keymap::is_safety_key(chord) {
            return Err(BindError::SafetyKey);
        }
        let previous = self
            .bindings
            .iter()
            .find(|(other, c)| **c == chord && **other != def.id && self.scope_of(other) == scope)
            .map(|(other, _)| *other);
        if let Some(other) = previous {
            self.bindings.remove(other);
            self.scopes.remove(other);
        }
        self.bindings.insert(def.id, chord);
        if scope == ShortcutScope::Global {
            self.scopes.remove(def.id);
        } else {
            self.scopes.insert(def.id, scope);
        }
        Ok(previous)
    }

    /// The scope of `id`'s user binding (global when it has none or it is global).
    #[must_use]
    pub fn scope_of(&self, id: &str) -> crate::editor_keymap::ShortcutScope {
        self.scopes.get(id).copied().unwrap_or_default()
    }

    /// Removes the user shortcut of `id`; whether it had one.
    pub fn unbind(&mut self, id: &str) -> bool {
        let before = self.bindings.len();
        self.bindings.retain(|k, _| *k != id);
        self.scopes.retain(|k, _| *k != id);
        before != self.bindings.len()
    }

    /// The shortcut shown for `def`: the user's when bound, else the built-in one.
    pub fn shortcut_of(&self, def: &CommandDef) -> String {
        self.bindings
            .get(def.id)
            .map_or_else(|| def.shortcut.to_owned(), KeyChord::label)
    }
}

/// The chord a key event records: the pressed key with the held modifiers, ignoring the keys
/// the palette itself uses while recording (Escape clears instead).
pub fn chord_of(key: egui::Key, modifiers: egui::Modifiers) -> KeyChord {
    KeyChord {
        command: modifiers.command,
        shift: modifiers.shift,
        alt: modifiers.alt,
        key,
    }
}

/// The palette's state.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct CommandPalette {
    /// Whether the window shows.
    pub open: bool,
    /// The text typed so far.
    pub query: String,
    /// Index of the highlighted row in [`Self::ranked`].
    pub selected: usize,
    /// Ids run most recently, newest first.
    pub recent: Vec<&'static str>,
    /// Ids the active feature profile hides (`crate::feature_profiles::hidden_command_ids`,
    /// ADR-7753): left out of every ranking.
    pub hidden: Vec<&'static str>,
    /// Rows of extension commands ranked with [`COMMANDS`], refreshed from the world before the
    /// palette draws and before `editor.command` ranks (`extension::actions::command_defs`,
    /// ADR-7791).
    pub extension_rows: Vec<CommandDef>,
}

/// One ranked row.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    /// The command.
    pub def: CommandDef,
    /// Score (higher is better).
    pub score: i32,
    /// Matched character positions in `category: title`.
    pub positions: Vec<usize>,
}

impl CommandPalette {
    /// The commands matching `query`, best first. With an empty query the recent commands come
    /// first (newest first), then the registry order.
    pub fn rank(&self, query: &str) -> Vec<Ranked> {
        let mut rows: Vec<(usize, Ranked)> = entries()
            .iter()
            .chain(self.extension_rows.iter())
            .enumerate()
            .filter(|(_, def)| !self.hidden.contains(&def.id))
            .filter_map(|(i, def)| {
                let text = format!("{}: {}", def.category, def.title);
                let (mut score, positions) = fuzzy_match(query, &text)?;
                if let Some(age) = self.recent.iter().position(|r| *r == def.id) {
                    score += RECENT_BONUS - (age as i32) * (RECENT_BONUS / RECENT_LEN as i32);
                }
                Some((
                    i,
                    Ranked {
                        def: *def,
                        score,
                        positions,
                    },
                ))
            })
            .collect();
        rows.sort_by(|a, b| b.1.score.cmp(&a.1.score).then(a.0.cmp(&b.0)));
        rows.into_iter().map(|(_, r)| r).collect()
    }

    /// The rows for the current query.
    pub fn ranked(&self) -> Vec<Ranked> {
        self.rank(&self.query)
    }

    /// Moves the highlight by `delta` rows, wrapping.
    pub fn step(&mut self, delta: isize) {
        let n = self.ranked().len();
        if n == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(n as isize) as usize;
    }

    /// Opens the palette with an empty query.
    pub fn open_fresh(&mut self) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
    }

    /// Remembers that `id` ran.
    pub fn note_run(&mut self, id: &'static str) {
        self.recent.retain(|r| *r != id);
        self.recent.insert(0, id);
        self.recent.truncate(RECENT_LEN);
    }
}

/// Whether the user gave command `id` a shortcut: the viewport then leaves the command's built-in
/// key alone, so the binding replaces it (ADR-2321).
#[must_use]
pub fn user_bound(world: &World, id: &str) -> bool {
    world
        .get_resource::<CommandKeymap>()
        .is_some_and(|k| k.bindings.contains_key(id))
}

/// The command called `id`.
pub fn find(id: &str) -> Option<CommandDef> {
    entries().iter().copied().find(|c| c.id == id)
}

/// Frames the selection in the editor camera.
pub fn frame_selection(world: &mut World) {
    let selected: Vec<Entity> = world
        .get_resource_or_insert_with::<Selection>(Selection::default)
        .entities()
        .to_vec();
    let positions: Vec<regia_core::math::Vec3> = selected
        .iter()
        .filter_map(|e| crate::viewport::world_position(world, *e))
        .collect();
    let mut camera = world
        .get_resource_or_insert_with::<EditorCamera>(EditorCamera::default)
        .clone();
    camera.frame(&positions);
    world.insert_resource(camera);
}

/// The outliner's selection commands, as its keys run them.
fn outliner_command(world: &mut World, id: &str) -> String {
    let selected: Vec<Entity> = world
        .get_resource::<Selection>()
        .map(|s| s.entities().to_vec())
        .unwrap_or_default();
    if selected.is_empty() {
        return "Nothing selected".to_owned();
    }
    let result = match id {
        "outliner.delete" => crate::hierarchy_ops::delete_entities(world, &selected)
            .map(|done| format!("Deleted {}", done.len())),
        "outliner.duplicate" => crate::hierarchy_ops::duplicate_entities(
            world,
            &selected,
            regia_core::math::Vec3::new(1.0, 0.0, 0.0),
        )
        .map(|done| format!("Duplicated {}", done.len())),
        _ => crate::hierarchy_ops::group_entities(world, &selected, "Group").map(|_| "Grouped".to_owned()),
    };
    result.unwrap_or_else(|err| err.to_string())
}

/// Runs the command `id` against the world and returns a status line. `None` for an unknown id.
/// An id no built-in command has runs the extension action of that id when one is loaded
/// (ADR-9850).
pub fn run(world: &mut World, id: &str) -> Option<String> {
    let Some(def) = find(id) else {
        let action = crate::extension::actions::actions(world)
            .iter()
            .any(|(_, action)| action.id == id);
        return action.then(|| match crate::extension::actions::run_action(world, id) {
            Ok((_, message)) => message,
            Err(error) => error.message,
        });
    };
    let message = match def.id {
        // A menu item listed in the palette (P-022, ADR-7766).
        id if id.starts_with(MENU_ENTRY_PREFIX) => run_menu_entry(world, id)?,
        // Universal search, favorites and the selection history (P-019, P-030, P-033).
        "search.everything" => {
            world
                .get_resource_or_insert_with(crate::universal_search::UniversalSearch::default)
                .open_fresh();
            "Search everything".to_owned()
        }
        "view.favorites" => {
            let mut favorites = world.get_resource_or_insert_with(PaletteFavorites::default);
            favorites.popup_open = !favorites.popup_open;
            favorites.selected = 0;
            format!(
                "Favorites {}",
                if favorites.popup_open { "shown" } else { "hidden" }
            )
        }
        "selection.back" | "selection.forward" => {
            let step = if def.id == "selection.back" {
                crate::selection::HistoryStep::Back
            } else {
                crate::selection::HistoryStep::Forward
            };
            match crate::selection::go(world, step) {
                Some(entry) => format!("Selected {}", crate::selection::snapshot_label(world, &entry)),
                None => "No selection that way".to_owned(),
            }
        }
        "selection.history" => {
            let mut favorites = world.get_resource_or_insert_with(PaletteFavorites::default);
            favorites.history_open = !favorites.history_open;
            format!(
                "Selection history {}",
                if favorites.history_open { "shown" } else { "hidden" }
            )
        }
        "review.add_note" => {
            crate::review_notes::begin_note(world);
            "Add review note".to_owned()
        }
        "edit.undo" => {
            if crate::command::undo_world(world) {
                "Undone".to_owned()
            } else {
                "Nothing to undo".to_owned()
            }
        }
        "edit.redo" => {
            if crate::command::redo_world(world) {
                "Redone".to_owned()
            } else {
                "Nothing to redo".to_owned()
            }
        }
        "view.frame_selection" => {
            frame_selection(world);
            "Framed the selection".to_owned()
        }
        "view.zoom_in" | "view.zoom_out" | "view.reset_zoom" => {
            let step = match def.id {
                "view.zoom_in" => crate::editor_prefs::ZoomStep::In,
                "view.zoom_out" => crate::editor_prefs::ZoomStep::Out,
                _ => crate::editor_prefs::ZoomStep::Reset,
            };
            let scale = crate::editor_prefs::zoom_ui_scale(world, step);
            format!("Interface zoom {:.0} %", scale * 100.0)
        }
        "view.toggle_grid" | "view.toggle_labels" | "view.toggle_realtime" | "view.toggle_audio_gizmos" => {
            let mut s = world
                .get_resource_or_insert_with::<ViewportSettings>(ViewportSettings::default)
                .clone();
            let (name, flag) = match def.id {
                "view.toggle_grid" => ("Grid", &mut s.grid),
                "view.toggle_labels" => ("Selection labels", &mut s.labels),
                "view.toggle_realtime" => ("Realtime viewport", &mut s.realtime),
                _ => ("Audio gizmos", &mut s.audio),
            };
            *flag = !*flag;
            let message = format!("{name} {}", if *flag { "on" } else { "off" });
            world.insert_resource(s);
            message
        }
        "gizmo.translate" | "gizmo.rotate" | "gizmo.scale" => {
            let (mode, message) = match def.id {
                "gizmo.translate" => (crate::viewport::GizmoMode::Translate, "Translate gizmo"),
                "gizmo.rotate" => (crate::viewport::GizmoMode::Rotate, "Rotate gizmo"),
                _ => (crate::viewport::GizmoMode::Scale, "Scale gizmo"),
            };
            world
                .get_resource_or_insert_with(crate::viewport::GizmoState::default)
                .mode = mode;
            message.to_owned()
        }
        "window.collaboration" => {
            crate::collab::toggle_window(world);
            "Collaboration window toggled".into()
        }
        // Shell actions (ADR-2321): queued for the shell's next frame.
        "window.preferences"
        | "window.project_settings"
        | "layout.save"
        | "layout.save_default"
        | "layout.reset" => {
            use crate::editor_layout::{LayoutRequest, LayoutRequests};
            let (request, message) = match def.id {
                "window.preferences" => (
                    LayoutRequest::Open(crate::shell::Panel::Preferences),
                    "Preferences",
                ),
                "window.project_settings" => (
                    LayoutRequest::Open(crate::shell::Panel::ProjectSettings),
                    "Project settings",
                ),
                "layout.save" => (LayoutRequest::SaveProject, "Saving the layout to the project"),
                "layout.save_default" => (LayoutRequest::SaveUser, "Saving the layout as your default"),
                _ => (LayoutRequest::Reset, "Layout reset"),
            };
            LayoutRequests::push(world, request);
            message.to_owned()
        }
        // A panel tab brought forward (added when closed) from the keyboard (ADR-9810).
        id if id.starts_with(OPEN_PANEL_PREFIX) => {
            use crate::editor_layout::{LayoutRequest, LayoutRequests};
            let panel = crate::shell::Panel::from_id(id.strip_prefix(OPEN_PANEL_PREFIX)?)?;
            LayoutRequests::push(world, LayoutRequest::Open(panel));
            format!("{} tab", panel.title())
        }
        "window.world_settings" => {
            use crate::editor_layout::{LayoutRequest, LayoutRequests};
            LayoutRequests::push(world, LayoutRequest::Open(crate::shell::Panel::WorldSettings));
            "World settings".to_owned()
        }
        "gizmo.vertex_snap" => {
            let mut options =
                world.get_resource_or_insert_with(crate::surface_tools::PlacementOptions::default);
            options.vertex_snap = !options.vertex_snap;
            format!("Vertex snap {}", if options.vertex_snap { "on" } else { "off" })
        }
        "selection.drop_to_floor" => crate::surface_tools::drop_selection(world),
        // ADR-7756: the select tool, the reference space, fixed-step rotate, surface snap.
        "gizmo.select" => {
            world
                .get_resource_or_insert_with(crate::viewport::GizmoState::default)
                .mode = crate::viewport::GizmoMode::Select;
            "Select tool".to_owned()
        }
        "gizmo.toggle_space" => {
            let mut gizmo = world.get_resource_or_insert_with(crate::viewport::GizmoState::default);
            gizmo.space = gizmo.space.next();
            format!("{} space", gizmo.space.label())
        }
        "gizmo.toggle_snap" => {
            let mut gizmo = world.get_resource_or_insert_with(crate::viewport::GizmoState::default);
            gizmo.snap = !gizmo.snap;
            format!("Snap {}", if gizmo.snap { "on" } else { "off" })
        }
        "gizmo.pivot_offset" => {
            let selected = world
                .get_resource::<Selection>()
                .map(|s| s.entities().to_vec())
                .unwrap_or_default();
            crate::viewport_ops::set_pivot_offset(world, &selected, regia_core::math::Vec3::ZERO);
            "Pivot offset cleared".to_owned()
        }
        "gizmo.rotate_step" | "gizmo.rotate_step_back" => {
            let sign = if def.id == "gizmo.rotate_step" { 1.0 } else { -1.0 };
            match crate::viewport_ops::rotate_step(world, sign) {
                Ok(outcome) => format!("Rotated {} by the angle step", outcome.moved.len()),
                Err(_) => "Nothing selected to rotate".to_owned(),
            }
        }
        "placement.toggle_surface_snap" | "placement.toggle_surface_align" => {
            let mut snap = world.get_resource_or_insert_with(crate::surface_tools::SurfaceSnap::default);
            let (name, flag) = if def.id == "placement.toggle_surface_snap" {
                ("Surface snap", &mut snap.enabled)
            } else {
                ("Align to surface while snapping", &mut snap.align)
            };
            *flag = !*flag;
            format!("{name} {}", if *flag { "on" } else { "off" })
        }
        // The Create prefab variant dialog, drawn by the shell (ADR-7744).
        "prefab.create_variant" => crate::prefab_variant_edit::request_dialog_for_selection(world),
        "placement.toggle_align_normal" => {
            let mut options =
                world.get_resource_or_insert_with(crate::surface_tools::PlacementOptions::default);
            options.align_to_normal = !options.align_to_normal;
            format!(
                "Align to surface normal {}",
                if options.align_to_normal { "on" } else { "off" }
            )
        }
        "collider.toggle_handles" => {
            let mut tool =
                world.get_resource_or_insert_with(crate::collider_handles::ColliderHandleTool::default);
            tool.enabled = !tool.enabled;
            format!("Collider handles {}", if tool.enabled { "on" } else { "off" })
        }
        "camera.fly_forward" | "camera.fly_back" | "camera.fly_left" | "camera.fly_right"
        | "camera.fly_up" | "camera.fly_down" => {
            "Hold this key with the right mouse button over the viewport".to_owned()
        }
        "outliner.delete" | "outliner.duplicate" | "outliner.group" => outliner_command(world, def.id),
        // The shell's menus as commands (ADR-7307): File → New level, Create → Shape, View → windows.
        "file.new_level" | "file.new_level_discard" => {
            // The File menu hides these while a prefab is open (the new level would unload the
            // prefab session's scene under PrefabEditMode's stash); the command must refuse too.
            if world
                .get_resource::<crate::prefab_edit::PrefabEditMode>()
                .is_some_and(|mode| mode.editing().is_some())
            {
                "Close the prefab editor first (editor.close_prefab) to switch levels".to_owned()
            } else {
                let unsaved = world
                    .get_resource::<crate::command::UndoStack>()
                    .is_some_and(crate::command::UndoStack::is_dirty);
                if unsaved && def.id == "file.new_level" {
                    "Unsaved changes: save first (editor.save_level), or run file.new_level_discard"
                        .to_owned()
                } else {
                    let open = crate::level_io::new_level(world);
                    world.insert_resource(open);
                    crate::level_io::reset_edit_state(world);
                    "New level".to_owned()
                }
            }
        }
        "recovery.restore" | "recovery.discard" => {
            use crate::editor_layout::{LayoutRequest, LayoutRequests};
            if def.id == "recovery.restore" {
                // Restoring an autosave replaces the open level: same prefab-mode hole as
                // file.new_level, so refuse while a prefab session is active.
                if world
                    .get_resource::<crate::prefab_edit::PrefabEditMode>()
                    .is_some_and(|mode| mode.editing().is_some())
                {
                    crate::autosave::RESTORE_IN_PREFAB_MODE.to_owned()
                } else {
                    LayoutRequests::push(world, LayoutRequest::RestoreRecovery);
                    "Restoring the offered autosave".to_owned()
                }
            } else {
                LayoutRequests::push(world, LayoutRequest::DiscardRecovery);
                "Discarding the offered autosave".to_owned()
            }
        }
        "viewport.toggle_spacemouse_object" => {
            let mut nav = world.get_resource_or_insert_with(crate::spacemouse::SpaceMouseNav::default);
            nav.object_tool = !nav.object_tool;
            format!(
                "SpaceMouse Object tool {}",
                if nav.object_tool { "on" } else { "off" }
            )
        }
        "material.undo" | "material.redo" | "material.reload" => {
            use crate::editor_layout::{LayoutRequest, LayoutRequests};
            let (request, message) = match def.id {
                "material.undo" => (LayoutRequest::MaterialUndo, "Material panel: undo"),
                "material.redo" => (LayoutRequest::MaterialRedo, "Material panel: redo"),
                _ => (LayoutRequest::MaterialReload, "Material panel: reload from disk"),
            };
            LayoutRequests::push(world, request);
            message.to_owned()
        }
        "terrain.retry_level_terrain" => {
            if world
                .remove_resource::<crate::terrain_panel::EditorLevelTerrain>()
                .is_some()
            {
                "Level terrain: loading again".to_owned()
            } else {
                "Level terrain: nothing to retry".to_owned()
            }
        }
        "lightmap.discard_checkpoint" => crate::lightmap_panel::discard_checkpoint(world),
        "help.context" => crate::about::help_context_command(world),
        "view.toggle_frame_debugger" => {
            let open = crate::frame_debugger::toggle(world);
            format!("Frame Debugger {}", if open { "shown" } else { "hidden" })
        }
        "view.toggle_profiler" => {
            let mut panel = world.get_resource_or_insert_with(crate::profiler_panel::ProfilerPanel::default);
            panel.open = !panel.open;
            format!("Profiler {}", if panel.open { "shown" } else { "hidden" })
        }
        "window.asset_jobs" => crate::asset_jobs_panel::toggle(world),
        "view.toggle_revision_control" => {
            let open = crate::vcs::panel::toggle(world);
            format!("Revision Control {}", if open { "shown" } else { "hidden" })
        }
        "view.toggle_file_locks" => {
            let open = crate::vcs::locks::toggle(world);
            format!("File Locks {}", if open { "shown" } else { "hidden" })
        }
        "vcs.unlock_all_mine" => match crate::vcs::locks::unlock_all_mine(world) {
            Ok(done) => format!("Unlocked {} file(s)", done.len()),
            Err(error) => format!("Unlock all mine: {} ({})", error.message, error.code),
        },
        "view.toggle_diagnostics" => {
            let mut panel =
                world.get_resource_or_insert_with(crate::diagnostics_panel::DiagnosticsPanel::default);
            panel.open = !panel.open;
            format!("Diagnostics {}", if panel.open { "shown" } else { "hidden" })
        }
        // The Overrides window and Edit in place for the selected prefab instance (ADR-7790).
        "prefab.overrides" | "prefab.edit_in_place" => {
            let selected = world.get_resource::<Selection>().and_then(|selection| {
                selection
                    .primary()
                    .or_else(|| selection.entities().first().copied())
            });
            match selected {
                None => "Select a prefab instance first".to_owned(),
                Some(entity) if def.id == "prefab.overrides" => {
                    crate::prefab_overrides_panel::open_for(world, entity);
                    "Prefab overrides shown".to_owned()
                }
                Some(entity) => match crate::prefab_edit::enter_prefab_edit_in_context(world, entity) {
                    Ok(path) => format!("Editing {} in place", path.display()),
                    Err(err) => format!("Edit in place failed: {err}"),
                },
            }
        }
        id if id.starts_with("create.") => {
            let kind = &id["create.".len()..];
            if world.get_resource::<AppTypeRegistry>().is_none() {
                "Create failed: no edit world (no type registry)".to_owned()
            } else {
                match crate::shapes::create_shape(world, kind) {
                    Ok(_) => format!("{} created", crate::shapes::shape_label(kind)),
                    Err(err) => format!("Create failed: {err}"),
                }
            }
        }
        "view.toggle_motion_database" => {
            let mut editor = world.get_resource_or_insert_with(crate::mm_db_editor::MmDbEditor::default);
            editor.ui.window_open = !editor.ui.window_open;
            format!(
                "Motion database editor {}",
                if editor.ui.window_open { "shown" } else { "hidden" }
            )
        }
        // Drawers (ADR-7754).
        "view.content_drawer" => crate::drawers::toggle(world, crate::drawers::DrawerKind::Content),
        "view.log_drawer" => crate::drawers::toggle(world, crate::drawers::DrawerKind::Log),
        "view.dock_drawer" => crate::drawers::dock_open(world),
        // Viewport camera (ADR-7755).
        id if crate::camera_nav::COMMAND_IDS.contains(&id) => crate::camera_nav::run_command(world, id)?,
        // Pilot, look through and screenshots (ADR-7768).
        "view.pilot" | "view.eject" | "view.look_through_next" | "view.editor_camera" => {
            crate::viewport_pilot::run_command(world, def.id)
        }
        "file.screenshot" => {
            crate::screenshot::open_dialog(world);
            "Screenshot…".to_owned()
        }
        "help.samples" => {
            crate::samples_browser::open_window(world, None);
            "Samples…".to_owned()
        }
        id if id.starts_with("selection.similar_") => {
            let by = crate::selection::SimilarBy::ALL
                .into_iter()
                .find(|by| by.command() == id)?;
            match crate::selection::select_similar(world, by) {
                Some(found) => format!("Selected {} ({})", found.len(), by.label().to_lowercase()),
                None => "Nothing selected to compare with".to_owned(),
            }
        }
        "selection.lasso" => {
            let mut shape = world.get_resource_or_insert_with(crate::selection::SelectionShape::default);
            shape.lasso = !shape.lasso;
            format!(
                "Selection drags draw a {}",
                if shape.lasso { "lasso" } else { "box" }
            )
        }
        "selection.pick_stack" => {
            world.insert_resource(crate::pick_stack::PickStackRequest);
            "Listing the objects under the cursor".to_owned()
        }
        "selection.toggle_root" => {
            let selected = world
                .get_resource::<Selection>()
                .map(|s| s.entities().to_vec())
                .unwrap_or_default();
            if selected.is_empty() {
                "Nothing selected".to_owned()
            } else {
                let on = !selected
                    .iter()
                    .all(|e| world.get::<crate::selection::EditorSelectionRoot>(*e).is_some());
                for entity in &selected {
                    crate::pick_stack::set_selection_root(world, *entity, on);
                }
                format!("Selection root {}", if on { "on" } else { "off" })
            }
        }
        "viewport.measure" => {
            let mut tool = world
                .get_resource::<crate::measure_tool::MeasureTool>()
                .cloned()
                .unwrap_or_default();
            let on = !tool.active;
            tool.set_active(on);
            world.insert_resource(tool);
            format!("Measure {}", if on { "on" } else { "off" })
        }
        // ADR-7770: the main viewport's active pane; the panes take Alt+1..8 directly.
        id if id.starts_with("view.mode_") => {
            let slot = id["view.mode_".len()..].parse::<usize>().ok()?.checked_sub(1)?;
            let mode = *crate::views::ViewMode::KEYED.get(slot)?;
            let mut layout = world.get_resource_or_insert_with(crate::views::ViewportLayout::default);
            let pane = layout.active;
            layout.display_mut(pane).view_mode = mode;
            format!("View mode {}", mode.label())
        }
        "view.hide_selection" => {
            let selected = world
                .get_resource::<crate::selection::Selection>()
                .map(|s| s.entities().to_vec())
                .unwrap_or_default();
            let n = crate::view_state::set_view_hidden(world, &selected, true);
            format!("Hid {n} in the editor")
        }
        "view.unhide_all" => {
            let n = crate::view_state::unhide_all(world);
            format!("Showed {n} hidden in the editor")
        }
        "view.toggle_rendering_debug" => {
            let mut panel =
                world.get_resource_or_insert_with(crate::render_debug_panel::RenderDebugPanel::default);
            panel.open = !panel.open;
            format!("Rendering Debug {}", if panel.open { "shown" } else { "hidden" })
        }
        "view.isolate" => match crate::view_state::toggle_isolate(world) {
            Ok(_) if crate::view_state::isolating(world) => "Isolated the selection".to_owned(),
            Ok(_) => "Isolate ended".to_owned(),
            Err(err) => err.code().to_owned(),
        },
        // Maximize, full screen and workspaces (ADR-7753).
        other => crate::workspaces::run_command(world, other)
            // Floated panels (ADR-7821).
            .or_else(|| crate::os_windows::run_command(world, other))
            .or_else(|| crate::panel_instances::run_command(world, other))?,
    };
    world
        .get_resource_or_insert_with::<CommandPalette>(CommandPalette::default)
        .note_run(def.id);
    Some(message)
}

/// Whether an IME composition is in progress: a non-empty pre-edit since the last commit or
/// dismissal (egui's empty pre-edit), kept across frames in the context's temp data. Without a
/// focused text field no composition is going on.
fn ime_composing(ctx: &egui::Context) -> bool {
    let id = egui::Id::new("regia_editor::command_palette::ime_composing");
    let mut composing = ctx.data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
    ctx.input(|i| {
        for event in &i.events {
            match event {
                egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) => composing = !text.is_empty(),
                egui::Event::Ime(egui::ImeEvent::Commit(_)) => composing = false,
                _ => {}
            }
        }
    });
    composing &= ctx.text_edit_focused();
    ctx.data_mut(|d| d.insert_temp(id, composing));
    composing
}

/// Draws the palette window when open, and handles Ctrl/Cmd+Shift+P. Returns a status line when a
/// command ran this frame. Not shown during play (`playing`), where the shortcut is ignored.
pub fn show(ctx: &egui::Context, world: &mut World, playing: bool) -> Option<String> {
    let mut keymap = world
        .get_resource_or_insert_with::<CommandKeymap>(CommandKeymap::default)
        .clone();
    crate::tooltips::sync_keymap(ctx, &keymap);
    crate::about::HoveredField::sync(ctx, world);
    // The Keymap tab's "Press a key…" search (ADR-7767): the next press is captured before any
    // shortcut runs; Escape cancels.
    let capturing = capture_chord(ctx, world);
    // Recording a chord for the highlighted command (F2 in the palette): the next key press with
    // its modifiers becomes the binding; Escape clears the binding instead.
    let mut recorded = false;
    if let Some(id) = keymap.recording {
        let pressed = ctx.input_mut(|i| {
            let mut found = None;
            i.events.retain(|e| match e {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } if found.is_none() => {
                    found = Some((*key, *modifiers));
                    false
                }
                // The chord's character must not land in the query field.
                egui::Event::Text(_) => found.is_none(),
                _ => true,
            });
            found
        });
        if let Some((key, modifiers)) = pressed {
            keymap.recording = None;
            recorded = true;
            if key == egui::Key::Escape {
                keymap.unbind(id);
            } else {
                // A reserved chord is refused; the old binding stays.
                let _ = keymap.bind(id, chord_of(key, modifiers));
            }
            world.insert_resource(keymap.clone());
        }
    }
    // User shortcuts run their command (not during play): the focused panel's scope first, then
    // global; while typing only text-scope bindings (and F1) run (ADR-7767).
    let mut status = None;
    if !recorded && !capturing && keymap.recording.is_none() && !playing {
        let scope = crate::editor_keymap::active_scope(world, ctx.text_edit_focused());
        let hit = crate::editor_keymap::dispatch_order(&keymap, scope)
            .into_iter()
            .find(|(_, chord)| ctx.input_mut(|i| i.consume_shortcut(&chord.shortcut())))
            .map(|(id, _)| id);
        // The global built-in shortcuts (the zoom keys) of commands the user did not rebind.
        // Ctrl+= zooms in too (egui's alternative to Ctrl+Plus on keyboards without a Plus key).
        let builtin = || {
            GLOBAL_SHORTCUT_COMMANDS
                .iter()
                .copied()
                .filter(|id| !keymap.bindings.contains_key(id))
                .find(|id| {
                    let chord = find(id).and_then(|d| KeyChord::parse(d.shortcut));
                    let equals = *id == "view.zoom_in"
                        && ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Equals));
                    equals || chord.is_some_and(|c| ctx.input_mut(|i| i.consume_shortcut(&c.shortcut())))
                })
        };
        if let Some(id) = hit.or_else(builtin) {
            status = run(world, id);
        }
        // Extension command shortcuts (ADR-7791); built-in chords were consumed first.
        if status.is_none() {
            status = crate::extension::actions::dispatch_shortcuts(ctx, world);
        }
    }
    if let Some(line) = shell_windows(ctx, world, &keymap, playing) {
        status = Some(line);
    }
    // Ctrl/Cmd+Shift+P works from any focus, a text field's included, except during an IME
    // composition: there the keys belong to the input method (ADR-8583).
    let toggle = !ime_composing(ctx)
        && ctx.input_mut(|i| {
            i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::P,
            ))
        });
    let mut palette = world
        .get_resource_or_insert_with::<CommandPalette>(CommandPalette::default)
        .clone();
    palette.hidden = crate::feature_profiles::hidden_command_ids(world);
    if palette.open || toggle {
        palette.extension_rows = crate::extension::actions::command_defs(world);
    }
    if toggle && !playing {
        if palette.open {
            palette.open = false;
        } else {
            palette.open_fresh();
        }
    }
    if !palette.open {
        world.insert_resource(palette);
        return status;
    }
    let (escape, down, up, enter, rebind) = ctx.input_mut(|i| {
        (
            i.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
            i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
            i.consume_key(egui::Modifiers::NONE, egui::Key::F2),
        )
    });
    // F2 rebinds built-in commands; extension commands are rebound in the Keymap tab (ADR-7791).
    if rebind
        && let Some(row) = palette.ranked().get(palette.selected)
        && find(row.def.id).is_some()
    {
        keymap.recording = Some(row.def.id);
        world.insert_resource(keymap.clone());
    }
    // F4 pins the highlighted row to the favorites (or unpins it).
    let pin = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F4));
    let mut favorites = world
        .get_resource_or_insert_with(PaletteFavorites::default)
        .clone();
    if pin && let Some(row) = palette.ranked().get(palette.selected) {
        let pinned = !favorites.contains(row.def.id);
        let _ = favorites.set(row.def.id, pinned);
        world.insert_resource(favorites.clone());
    }
    if escape {
        palette.open = false;
    }
    if down {
        palette.step(1);
    }
    if up {
        palette.step(-1);
    }
    let mut chosen: Option<&'static str> = None;
    if palette.open {
        egui::Window::new("Command palette")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 60.0))
            .show(ctx, |ui| {
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut palette.query)
                        .hint_text("Type a command")
                        .desired_width(380.0),
                );
                edit.request_focus();
                if edit.changed() {
                    palette.selected = 0;
                }
                let rows = palette.ranked();
                palette.selected = palette.selected.min(rows.len().saturating_sub(1));
                for (i, row) in rows.iter().take(WINDOW_ROWS).enumerate() {
                    let shortcut = if keymap.recording == Some(row.def.id) {
                        "press a key (Esc clears)".to_owned()
                    } else {
                        keymap.shortcut_of(&row.def)
                    };
                    let label = row_label(&row.def, &shortcut, favorites.contains(row.def.id));
                    let tip = crate::tooltips::RichTooltip::for_command(&row.def, &keymap);
                    if ui
                        .selectable_label(i == palette.selected, label)
                        .on_hover_ui(|ui| tip.show(ui))
                        .clicked()
                    {
                        chosen = Some(row.def.id);
                    }
                }
                if rows.is_empty() {
                    ui.weak("No command matches");
                }
                ui.weak("F2: change the highlighted command's shortcut   F4: pin to or unpin from favorites");
                if enter && let Some(row) = rows.get(palette.selected) {
                    chosen = Some(row.def.id);
                }
            });
    }
    if let Some(id) = chosen {
        palette.open = false;
        world.insert_resource(palette);
        status = run(world, id);
    } else {
        world.insert_resource(palette);
    }
    status
}

/// Takes the next key press for the Keymap tab's "Press a key…" search while
/// [`crate::editor_keymap::ChordCapture`] is active (Escape cancels). Answers whether a capture was
/// pending this frame, so no shortcut runs on the captured press.
fn capture_chord(ctx: &egui::Context, world: &mut World) -> bool {
    let Some(mut capture) = world
        .get_resource::<crate::editor_keymap::ChordCapture>()
        .copied()
        .filter(|c| c.active)
    else {
        return false;
    };
    // The Keymap tab stopped drawing the capture (another tab, the window closed): drop it.
    if ctx.cumulative_frame_nr() > capture.armed_frame + 1 {
        capture.active = false;
        world.insert_resource(capture);
        return false;
    }
    let pressed = ctx.input_mut(|i| {
        let mut found = None;
        i.events.retain(|e| match e {
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } if found.is_none() => {
                found = Some((*key, *modifiers));
                false
            }
            egui::Event::Text(_) => found.is_none(),
            _ => true,
        });
        found
    });
    if let Some((key, modifiers)) = pressed {
        capture.active = false;
        if key != egui::Key::Escape {
            capture.captured = Some(chord_of(key, modifiers));
        }
        world.insert_resource(capture);
    }
    true
}

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

fn ranked_json(rows: &[Ranked], limit: usize, keymap: &CommandKeymap) -> Vec<Value> {
    rows.iter()
        .take(limit)
        .map(|r| {
            json!({
                "id": r.def.id,
                "category": r.def.category,
                "title": r.def.title,
                "shortcut": keymap.shortcut_of(&r.def),
                "default_shortcut": r.def.shortcut,
                "menu_path": menu_path(r.def.id),
                "score": r.score,
                "positions": r.positions,
            })
        })
        .collect()
}

/// `editor.command { query?, limit?, run? }`.
fn editor_command(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bad = |code: &str, message: &str, hint: &str| RpcError::new(INVALID_PARAMS, code, message, hint);
    let query = match params.get("query") {
        None | Some(Value::Null) => "",
        Some(Value::String(s)) => s.as_str(),
        Some(_) => {
            return Err(bad(
                "bad_query",
                "`query` must be a string",
                "the text typed into the palette",
            ));
        }
    };
    let limit = match params.get("limit") {
        None | Some(Value::Null) => 20,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=200).contains(n))
            .map(|n| n as usize)
            .ok_or_else(|| {
                bad(
                    "bad_limit",
                    "`limit` must be a whole number from 1 to 200",
                    "20 is the default",
                )
            })?,
    };
    // `bind: { id, keys }` sets a user shortcut (`keys: null` removes it), before any `run`.
    let mut rebound = Value::Null;
    if let Some(b) = params.get("bind").filter(|v| !v.is_null()) {
        let id = b.get("id").and_then(Value::as_str).ok_or_else(|| {
            bad(
                "bad_binding",
                "`bind.id` must be a command id",
                "bind: { id: \"view.toggle_grid\", keys: \"Ctrl+G\" }",
            )
        })?;
        let mut keymap = world.get_resource::<CommandKeymap>().cloned().unwrap_or_default();
        let taken_from = match b.get("keys") {
            None | Some(Value::Null) => {
                if find(id).is_none() {
                    return Err(bad(
                        "unknown_command",
                        "no command has that id",
                        "list the ids with {query: \"\"}",
                    ));
                }
                keymap.unbind(id);
                None
            }
            Some(k) => {
                let chord = k.as_str().and_then(KeyChord::parse).ok_or_else(|| {
                    bad(
                        "bad_binding",
                        "`bind.keys` must be a chord like \"Ctrl+Shift+K\"",
                        "modifiers Ctrl (Cmd), Alt, Shift, then one key name such as K, F5 or Space",
                    )
                })?;
                keymap.bind(id, chord).map_err(|e| match e {
                    BindError::UnknownCommand => bad(
                        "unknown_command",
                        "no command has that id",
                        "list the ids with {query: \"\"}",
                    ),
                    BindError::Reserved => bad(
                        "bad_binding",
                        "Ctrl+Shift+P opens the palette and cannot be rebound",
                        "pick another chord",
                    ),
                    BindError::SafetyKey => bad(
                        "shortcut_safety_key",
                        "stop play, save, undo and redo stay global",
                        "bind it in the global scope, or pick another chord",
                    ),
                })?
            }
        };
        rebound = json!({
            "id": id,
            "keys": keymap.bindings.get(id).map(KeyChord::label),
            "taken_from": taken_from,
        });
        world.insert_resource(keymap);
    }
    // `pin: { id, pinned? }` pins an entry to the favorites (F4 in the palette), before any `run`.
    let mut pinned = Value::Null;
    if let Some(p) = params.get("pin").filter(|v| !v.is_null()) {
        let hint = "pin: { id: \"view.toggle_grid\", pinned: true }";
        let id = p
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("bad_pin", "`pin.id` must be a palette entry id", hint))?;
        let on = match p.get("pinned") {
            None | Some(Value::Null) => true,
            Some(v) => v
                .as_bool()
                .ok_or_else(|| bad("bad_pin", "`pin.pinned` must be a boolean", hint))?,
        };
        let mut favorites = world
            .get_resource::<PaletteFavorites>()
            .cloned()
            .unwrap_or_default();
        favorites.set(id, on).map_err(|_| {
            bad(
                "unknown_command",
                "no palette entry has that id",
                "list the ids with {query: \"\"}",
            )
        })?;
        pinned = json!({ "id": id, "pinned": on });
        world.insert_resource(favorites);
    }
    let mut ran = Value::Null;
    if let Some(v) = params.get("run").filter(|v| !v.is_null()) {
        let id = v.as_str().ok_or_else(|| {
            bad(
                "unknown_command",
                "`run` must be a command id",
                "list them with {query: \"\"}",
            )
        })?;
        let message = run(world, id).ok_or_else(|| {
            bad(
                "unknown_command",
                "no command has that id",
                "list the ids with {query: \"\"}",
            )
        })?;
        ran = json!({ "id": id, "message": message });
    }
    let mut palette = world
        .get_resource::<CommandPalette>()
        .cloned()
        .unwrap_or_default();
    palette.hidden = crate::feature_profiles::hidden_command_ids(world);
    palette.extension_rows = crate::extension::actions::command_defs(world);
    let rows = palette.rank(query);
    let keymap = world.get_resource::<CommandKeymap>().cloned().unwrap_or_default();
    Ok(json!({
        "query": query,
        "ran": ran,
        "bound": rebound,
        "count": rows.len(),
        "commands": ranked_json(&rows, limit, &keymap),
        "recent": palette.recent,
        "open": palette.open,
        "pinned": pinned,
        "favorites": world.get_resource::<PaletteFavorites>().map(|f| f.ids.clone()).unwrap_or_default(),
    }))
}

/// The palette's agent methods, installed by `agent_ext::all_methods`.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.command",
            tool: "editor_command",
            description: "The editor command palette: lists the commands matching a fuzzy query (best first, recently used commands rank higher) and can run one by id, exactly as picking it in the palette window (Ctrl+Shift+P) does. Every menu item is an entry too (id menu:<Menu/Label>), and each entry shows its menu path. bind sets a user shortcut for a command (F2 in the palette does the same); pin adds an entry to the favorites popup (F4).",
            params: &[
                ParamSchema {
                    name: "query",
                    kind: "string",
                    required: false,
                    description: "Fuzzy query over \"Category: Title\"; empty lists everything.",
                },
                ParamSchema {
                    name: "limit",
                    kind: "integer",
                    required: false,
                    description: "Most rows to return, 1 to 200 (default 20).",
                },
                ParamSchema {
                    name: "run",
                    kind: "string",
                    required: false,
                    description: "A command id (edit.undo, view.toggle_grid, ...) to run first.",
                },
                ParamSchema {
                    name: "bind",
                    kind: "object",
                    required: false,
                    description: "{ id, keys }: give a command a user shortcut such as \"Ctrl+Shift+G\" (taken off any other command); keys null removes it. Applied before run.",
                },
                ParamSchema {
                    name: "pin",
                    kind: "object",
                    required: false,
                    description: "{ id, pinned? }: pin an entry to the favorites popup (Ctrl+Shift+F), or unpin it with pinned false (F4 in the palette). Applied before run.",
                },
            ],
            result: "{ query, ran: { id, message } | null, bound: { id, keys, taken_from } | null, count, commands: [{ id, category, title, shortcut, default_shortcut, menu_path, score, positions }], recent, open, pinned: { id, pinned } | null, favorites }",
            mutating: true,
            permission: Permission::EditScene,
        },
        editor_command as Handler,
    )]
}

// ---------------------------------------------------------------------------------------------
// Menu paths, menu items as palette entries, and favorites (P-022, P-030, ADR-7766).

/// The id prefix of a palette entry made from a menu item: `menu:` and the item's `Menu/Label`
/// path (`menu:File/Open level…`).
pub const MENU_ENTRY_PREFIX: &str = "menu:";

/// How a menu path is shown: `Edit ▸ Duplicate`.
pub const MENU_PATH_SEPARATOR: &str = " ▸ ";

/// The palette's entries and their menu paths, built once from [`COMMANDS`] and the menu tree.
struct PaletteIndex {
    /// [`COMMANDS`], then one entry per menu item that is not already a command.
    entries: Vec<CommandDef>,
    /// Entry id → menu path (`Edit ▸ Duplicate`), for entries a menu shows.
    menu_paths: std::collections::HashMap<&'static str, String>,
}

fn leak(text: String) -> &'static str {
    // Built once per process (a few dozen menu items): the entries must be `CommandDef`s, whose
    // fields are `&'static str`.
    Box::leak(text.into_boxed_str())
}

fn palette_index() -> &'static PaletteIndex {
    static INDEX: std::sync::OnceLock<PaletteIndex> = std::sync::OnceLock::new();
    INDEX.get_or_init(|| {
        let mut entries: Vec<CommandDef> = COMMANDS.to_vec();
        let mut menu_paths = std::collections::HashMap::new();
        let mut titles: std::collections::HashSet<String> = COMMANDS
            .iter()
            .map(|c| format!("{}: {}", c.category, c.title))
            .collect();
        for item in crate::menus::menu_tree() {
            let shown = item.path().replace('/', MENU_PATH_SEPARATOR);
            let command = if item.method == "editor.command" {
                item.params.get("run").and_then(Value::as_str).map(str::to_owned)
            } else {
                match item.action.path {
                    crate::agent_parity::ActionPath::Command(id) => Some(id.to_owned()),
                    _ => None,
                }
            };
            if let Some(def) = command.and_then(|id| COMMANDS.iter().find(|c| c.id == id)) {
                menu_paths.entry(def.id).or_insert(shown);
                continue;
            }
            // A menu item whose action id is already a command (Help ▸ Samples… is
            // `help.samples`, a Method path, not `editor.command`) uses that command.
            if let Some(def) = COMMANDS.iter().find(|c| c.id == item.action.id) {
                menu_paths.entry(def.id).or_insert(shown);
                continue;
            }
            let title = item.label.replace('/', MENU_PATH_SEPARATOR);
            // One registry, no duplicate titles: a menu item that reads like an entry already
            // listed is not added again.
            if !titles.insert(format!("{}: {title}", item.menu)) {
                continue;
            }
            let id = leak(format!("{MENU_ENTRY_PREFIX}{}", item.path()));
            entries.push(CommandDef {
                id,
                category: item.menu,
                title: leak(title),
                shortcut: "",
            });
            menu_paths.insert(id, shown);
        }
        PaletteIndex { entries, menu_paths }
    })
}

/// Every palette entry: [`COMMANDS`], then each menu item that is not one of them (id
/// [`MENU_ENTRY_PREFIX`] + its path), so the palette offers every menu action.
#[must_use]
pub fn entries() -> &'static [CommandDef] {
    &palette_index().entries
}

/// The menu path of entry `id` (`Edit ▸ Duplicate`), when a menu shows it.
#[must_use]
pub fn menu_path(id: &str) -> Option<&'static str> {
    palette_index().menu_paths.get(id).map(String::as_str)
}

/// Runs a menu-item entry the way its menu item does: through the agent call `editor.menu_list`
/// names for it. Items that open a file dialog or ask for a confirmation are listed but not run
/// from the palette (the menu shows the dialog). `None` for an unknown menu path.
fn run_menu_entry(world: &mut World, id: &str) -> Option<String> {
    let path = id.strip_prefix(MENU_ENTRY_PREFIX)?;
    let item = crate::menus::menu_tree().into_iter().find(|i| i.path() == path)?;
    let shown = item.path().replace('/', MENU_PATH_SEPARATOR);
    let dialog = matches!(
        item.action.path,
        crate::agent_parity::ActionPath::OsDialog(_) | crate::agent_parity::ActionPath::Confirm(_)
    ) || item.params.get("confirm").and_then(Value::as_bool) == Some(true);
    if dialog {
        return Some(format!("{shown} opens a dialog: choose it from the menu"));
    }
    if !crate::menus::item_enabled(world, &item) {
        return Some(format!("{shown} is not available now"));
    }
    if item.method == "editor.command" {
        let command = item.params.get("run").and_then(Value::as_str)?.to_owned();
        return run(world, &command);
    }
    let Some((_, handler)) = crate::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == item.method)
    else {
        return Some(format!("{shown}: nothing to run"));
    };
    Some(match handler(world, &item.params) {
        Ok(_) => shown,
        Err(err) => format!("{shown}: {}", err.message),
    })
}

/// Pinned palette entries (P-030): the favorites popup (`view.favorites`, Ctrl+Shift+F) lists them.
/// Saved per user with the personal saved searches (`crate::saved_searches`).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct PaletteFavorites {
    /// Pinned entry ids, in pin order.
    pub ids: Vec<&'static str>,
    /// Whether the favorites popup shows.
    pub popup_open: bool,
    /// The highlighted favorite in the popup.
    pub selected: usize,
    /// Whether the selection history window (`selection.history`) shows.
    pub history_open: bool,
    /// Bumped by every pin or unpin (the personal file is written when it moves).
    pub generation: u64,
}

impl PaletteFavorites {
    /// Pins (`pinned`) or unpins entry `id`. `Err` for an unknown id; otherwise whether the pins
    /// changed.
    pub fn set(&mut self, id: &str, pinned: bool) -> Result<bool, BindError> {
        let def = find(id).ok_or(BindError::UnknownCommand)?;
        let has = self.ids.contains(&def.id);
        if has == pinned {
            return Ok(false);
        }
        if pinned {
            self.ids.push(def.id);
        } else {
            self.ids.retain(|i| *i != def.id);
        }
        self.selected = self.selected.min(self.ids.len().saturating_sub(1));
        self.generation += 1;
        Ok(true)
    }

    /// Whether entry `id` is pinned.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains(&id)
    }
}

/// The text a palette row shows: `Category: Title`, the menu path and the shortcut, and a star when
/// pinned.
#[must_use]
pub fn row_label(def: &CommandDef, shortcut: &str, pinned: bool) -> String {
    let mut label = format!(
        "{}{}: {}",
        if pinned { "★ " } else { "" },
        def.category,
        def.title
    );
    if let Some(path) = menu_path(def.id) {
        label.push_str("    ");
        label.push_str(path);
    }
    if !shortcut.is_empty() {
        label.push_str("    ");
        label.push_str(shortcut);
    }
    label
}

/// The favorites popup: the pinned entries; Up/Down move, Enter runs, Delete unpins, Escape
/// closes. Returns the status line of an entry it ran.
pub fn favorites_popup(ctx: &egui::Context, world: &mut World) -> Option<String> {
    if !world
        .get_resource::<PaletteFavorites>()
        .is_some_and(|f| f.popup_open)
    {
        return None;
    }
    let mut favorites = world.get_resource::<PaletteFavorites>().cloned()?;
    let (escape, down, up, enter, delete) = ctx.input_mut(|i| {
        (
            i.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
            i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
            i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
            i.consume_key(egui::Modifiers::NONE, egui::Key::Delete),
        )
    });
    let n = favorites.ids.len();
    if n > 0 {
        if down {
            favorites.selected = (favorites.selected + 1) % n;
        }
        if up {
            favorites.selected = (favorites.selected + n - 1) % n;
        }
    }
    let mut chosen = None;
    if enter {
        chosen = favorites.ids.get(favorites.selected).copied();
    }
    if delete && let Some(id) = favorites.ids.get(favorites.selected).copied() {
        let _ = favorites.set(id, false);
    }
    let keymap = world.get_resource::<CommandKeymap>().cloned().unwrap_or_default();
    egui::Window::new("Favorites")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 60.0))
        .show(ctx, |ui| {
            if favorites.ids.is_empty() {
                ui.weak("No favorites: press F4 on a palette row (Ctrl+Shift+P) to pin it");
            }
            for (i, id) in favorites.ids.clone().iter().enumerate() {
                let Some(def) = find(id) else {
                    continue;
                };
                let label = row_label(&def, &keymap.shortcut_of(&def), false);
                if ui.selectable_label(i == favorites.selected, label).clicked() {
                    chosen = Some(def.id);
                }
            }
            ui.weak("Enter: run   Delete: unpin   Esc: close");
        });
    if escape || chosen.is_some() {
        favorites.popup_open = false;
    }
    world.insert_resource(favorites);
    chosen.and_then(|id| run(world, id))
}

/// The selection history window (`selection.history`).
fn history_window(ctx: &egui::Context, world: &mut World) {
    let open = world
        .get_resource::<PaletteFavorites>()
        .is_some_and(|f| f.history_open);
    if !open {
        return;
    }
    let mut still_open = true;
    egui::Window::new("Selection history")
        .open(&mut still_open)
        .collapsible(false)
        .show(ctx, |ui| crate::selection::history_list(ui, world));
    if !still_open {
        world.resource_mut::<PaletteFavorites>().history_open = false;
    }
}

/// The windows the palette's commands open, and their default keys: universal search (Ctrl+T),
/// favorites (Ctrl+Shift+F) and the selection history (Alt+Left / Alt+Right). A user shortcut on
/// one of these commands replaces its default key (the keymap loop in [`show`] runs it).
fn shell_windows(
    ctx: &egui::Context,
    world: &mut World,
    keymap: &CommandKeymap,
    playing: bool,
) -> Option<String> {
    let mut status = None;
    if !playing && keymap.recording.is_none() {
        for id in ["search.everything", "view.favorites"] {
            let Some(chord) = find(id).and_then(|d| KeyChord::parse(d.shortcut)) else {
                continue;
            };
            if !keymap.bindings.contains_key(id) && ctx.input_mut(|i| i.consume_shortcut(&chord.shortcut())) {
                status = run(world, id);
            }
        }
    }
    let user_history_keys = ["selection.back", "selection.forward"]
        .iter()
        .any(|id| keymap.bindings.contains_key(id));
    if !user_history_keys && let Some(step) = crate::selection::history_keys(ctx, world, playing) {
        status = Some(format!("Selection history: {step:?}"));
    }
    crate::selection::track(world);
    if let Some(line) = crate::universal_search::show(ctx, world, playing) {
        status = Some(line);
    }
    if !playing && let Some(line) = favorites_popup(ctx, world) {
        status = Some(line);
    }
    history_window(ctx, world);
    crate::saved_searches::persist_personal(world);
    status
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn every_guide_panel_opens_from_the_palette() {
        use crate::editor_layout::{LayoutRequest, LayoutRequests};
        let mut world = World::new();
        let ids: Vec<&str> = COMMANDS
            .iter()
            .filter_map(|c| c.id.strip_prefix(OPEN_PANEL_PREFIX))
            .collect();
        for id in [
            "viewport",
            "outliner",
            "inspector",
            "content",
            "material_graph",
            "seq_curves",
            "ui_designer",
            "fracture",
            "proc_graph",
        ] {
            assert!(ids.contains(&id), "no window.open_{id}");
        }
        for id in &ids {
            let panel = crate::shell::Panel::from_id(id).unwrap();
            let line = run(&mut world, &format!("{OPEN_PANEL_PREFIX}{id}")).unwrap();
            assert_eq!(line, format!("{} tab", panel.title()));
            let queued = world.resource::<LayoutRequests>();
            assert_eq!(queued.pending.last(), Some(&LayoutRequest::Open(panel)), "{id}");
        }
    }

    /// ADR-7744: every built-in panel has its `window.open_<id>` command, generated from the panel
    /// list, and the panel list has every `Panel` variant (a new variant fails here until it is
    /// listed, and then it has a command).
    #[test]
    fn every_panel_variant_has_an_open_command() {
        // The variants of `pub enum Panel` in shell.rs, read from the source.
        let source = include_str!("shell.rs");
        let body = source
            .split_once("pub enum Panel {")
            .and_then(|(_, rest)| rest.split_once("\n}\n"))
            .map(|(body, _)| body)
            .unwrap();
        let variants: Vec<&str> = body
            .lines()
            .filter(|line| {
                line.strip_prefix("    ")
                    .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_uppercase()))
            })
            .collect();
        // `ExtensionPanel` opens by extension id (`editor.layout_apply`), not by a built-in id;
        // `GameView` is one PIE process's tab, opened when that process starts and never saved in a
        // layout (ADR-7831), so it has no built-in id to open either.
        let extension = variants.iter().filter(|v| v.contains("ExtensionPanel")).count();
        assert_eq!(extension, 1, "{variants:?}");
        let game_view = variants
            .iter()
            .filter(|v| v.trim_start().starts_with("GameView("))
            .count();
        assert_eq!(game_view, 1, "{variants:?}");
        // `UtilityPanel` opens by utility panel id (`editor.utility_panels { op: "open" }`,
        // ADR-8189) and is never saved in a layout either.
        let utility = variants
            .iter()
            .filter(|v| v.trim_start().starts_with("UtilityPanel("))
            .count();
        assert_eq!(utility, 1, "{variants:?}");
        // `Instance` opens by `editor.panel_new_instance` (ADR-8145). Tabs are
        // `outliner#2`, not one built-in id, so they are not in BUILT_IN_PANELS.
        let instance = variants
            .iter()
            .filter(|v| v.trim_start().starts_with("Instance("))
            .count();
        assert_eq!(instance, 1, "{variants:?}");
        assert_eq!(
            crate::editor_layout::BUILT_IN_PANELS.len(),
            variants.len() - extension - game_view - utility - instance,
            "a Panel variant is missing from BUILT_IN_PANELS (editor_layout.rs): add it there"
        );
        let mut world = World::new();
        for panel in crate::editor_layout::BUILT_IN_PANELS {
            let id = format!("{OPEN_PANEL_PREFIX}{}", panel.id());
            let def = find(&id).unwrap_or_else(|| panic!("no command {id}"));
            assert_eq!(def.title, format!("Open the {} tab", panel.title()));
            assert_eq!(run(&mut world, &id).unwrap(), format!("{} tab", panel.title()));
        }
        assert_eq!(
            COMMANDS
                .iter()
                .filter(|c| c.id.starts_with(OPEN_PANEL_PREFIX))
                .count(),
            crate::editor_layout::BUILT_IN_PANELS.len()
        );
    }

    #[test]
    fn registry_ids_are_unique_and_runnable() {
        let mut seen = std::collections::HashSet::new();
        let mut world = World::new();
        for c in COMMANDS.iter() {
            assert!(seen.insert(c.id), "duplicate {}", c.id);
            assert!(run(&mut world, c.id).is_some(), "{} has no implementation", c.id);
        }
        assert!(run(&mut world, "nope").is_none());
    }

    #[test]
    fn query_ranks_and_recents_float_up() {
        let mut p = CommandPalette::default();
        let rows = p.rank("grid");
        assert_eq!(rows[0].def.id, "view.toggle_grid");
        let all = p.rank("");
        // Every entry: the registry and the menu items that are not commands (ADR-7766).
        assert_eq!(all.len(), entries().len());
        assert_eq!(all[0].def.id, COMMANDS[0].id, "registry order without recents");
        p.note_run("view.toggle_realtime");
        assert_eq!(p.rank("")[0].def.id, "view.toggle_realtime");
        assert!(p.rank("zzzz").is_empty());
    }

    #[test]
    fn recents_are_deduplicated_and_capped() {
        let mut p = CommandPalette::default();
        for id in ["a", "b", "c", "d", "e", "f", "a"] {
            p.note_run(id);
        }
        assert_eq!(p.recent, ["a", "f", "e", "d", "c"]);
    }

    #[test]
    fn step_wraps() {
        let mut p = CommandPalette::default();
        p.step(-1);
        assert_eq!(p.selected, entries().len() - 1);
        p.step(1);
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn toggle_commands_flip_viewport_settings() {
        let mut world = World::new();
        let before = ViewportSettings::default().grid;
        assert_eq!(
            run(&mut world, "view.toggle_grid").as_deref(),
            Some(if before { "Grid off" } else { "Grid on" })
        );
        assert_eq!(world.resource::<ViewportSettings>().grid, !before);
        assert!(
            world
                .resource::<CommandPalette>()
                .recent
                .contains(&"view.toggle_grid")
        );
    }

    #[test]
    fn agent_method_lists_and_runs() {
        let mut world = World::new();
        let table = methods();
        let (schema, handler) = &table[0];
        assert_eq!(schema.name, "editor.command");
        let out = handler(&mut world, &json!({ "query": "undo" })).unwrap();
        assert_eq!(out["commands"][0]["id"], "edit.undo");
        let out = editor_command(&mut world, &json!({ "run": "edit.undo", "limit": 2 })).unwrap();
        assert_eq!(out["ran"]["id"], "edit.undo");
        assert_eq!(out["recent"][0], "edit.undo");
        assert_eq!(out["commands"].as_array().unwrap().len(), 2);
        let code = |e: RpcError| {
            e.data
                .and_then(|d| d["code"].as_str().map(str::to_owned))
                .unwrap_or_default()
        };
        assert_eq!(
            code(editor_command(&mut world, &json!({ "run": "nope" })).unwrap_err()),
            "unknown_command"
        );
        assert_eq!(
            code(editor_command(&mut world, &json!({ "limit": 0 })).unwrap_err()),
            "bad_limit"
        );
        assert_eq!(
            code(editor_command(&mut world, &json!({ "query": 3 })).unwrap_err()),
            "bad_query"
        );
    }

    #[test]
    fn chords_parse_label_and_round_trip() {
        let c = KeyChord::parse("Ctrl+Shift+K").unwrap();
        assert!(c.command && c.shift && !c.alt);
        assert_eq!(c.key, egui::Key::K);
        assert_eq!(c.label(), "Ctrl+Shift+K");
        assert_eq!(KeyChord::parse(&c.label()), Some(c));
        assert_eq!(
            KeyChord::parse("alt+f5").map(|c| c.label()),
            Some("Alt+F5".into())
        );
        assert_eq!(KeyChord::parse("cmd+g").map(|c| c.key), Some(egui::Key::G));
        assert!(KeyChord::parse("Ctrl+Ctrl+K").is_none());
        assert!(KeyChord::parse("Hyper+K").is_none());
        assert!(KeyChord::parse("Ctrl+NotAKey").is_none());
        assert!(KeyChord::parse("").is_none());
    }

    #[test]
    fn keymap_binds_moves_and_refuses_the_palette_chord() {
        let mut k = CommandKeymap::default();
        let g = KeyChord::parse("Ctrl+G").unwrap();
        assert_eq!(k.bind("view.toggle_grid", g), Ok(None));
        let def = find("view.toggle_grid").unwrap();
        assert_eq!(k.shortcut_of(&def), "Ctrl+G");
        // The same chord on another command moves it.
        assert_eq!(k.bind("view.toggle_labels", g), Ok(Some("view.toggle_grid")));
        assert_eq!(k.shortcut_of(&def), "", "back to the (empty) built-in");
        assert_eq!(k.bind("nope", g), Err(BindError::UnknownCommand));
        assert_eq!(
            k.bind("view.toggle_grid", KeyChord::parse("Ctrl+Shift+P").unwrap()),
            Err(BindError::Reserved)
        );
        assert!(k.unbind("view.toggle_labels"));
        assert!(!k.unbind("view.toggle_labels"));
        let undo = find("edit.undo").unwrap();
        assert_eq!(k.shortcut_of(&undo), "Ctrl+Z", "built-in shown when unbound");
    }

    #[test]
    fn agent_binds_and_lists_user_shortcuts() {
        let mut world = World::new();
        let out = editor_command(
            &mut world,
            &json!({ "bind": { "id": "view.toggle_grid", "keys": "Ctrl+Alt+G" }, "query": "grid" }),
        )
        .unwrap();
        assert_eq!(out["bound"]["keys"], "Ctrl+Alt+G");
        assert_eq!(out["commands"][0]["shortcut"], "Ctrl+Alt+G");
        assert_eq!(out["commands"][0]["default_shortcut"], "");
        let code = |e: RpcError| {
            e.data
                .and_then(|d| d["code"].as_str().map(str::to_owned))
                .unwrap_or_default()
        };
        assert_eq!(
            code(
                editor_command(
                    &mut world,
                    &json!({ "bind": { "id": "view.toggle_grid", "keys": "Ctrl+Shift+P" } })
                )
                .unwrap_err()
            ),
            "bad_binding"
        );
        assert_eq!(
            code(
                editor_command(&mut world, &json!({ "bind": { "id": "x", "keys": "Ctrl+G" } })).unwrap_err()
            ),
            "unknown_command"
        );
        let out = editor_command(
            &mut world,
            &json!({ "bind": { "id": "view.toggle_grid", "keys": null } }),
        )
        .unwrap();
        assert!(out["bound"]["keys"].is_null());
        assert!(world.resource::<CommandKeymap>().bindings.is_empty());
    }

    #[test]
    fn entries_show_their_menu_path_and_menu_items_are_entries_once() {
        assert_eq!(menu_path("edit.undo"), Some("Edit ▸ Undo"));
        assert_eq!(menu_path("view.toggle_grid"), Some("View ▸ Grid"));
        assert_eq!(menu_path("create.cuboid"), Some("Create ▸ Shape ▸ Cuboid"));
        assert_eq!(menu_path("window.preferences"), Some("Edit ▸ Preferences…"));
        assert_eq!(menu_path("gizmo.translate"), None, "not in a menu");
        // Every menu item is reachable from the palette: as its command, or as its own entry.
        for item in crate::menus::menu_tree() {
            let shown = item.path().replace('/', MENU_PATH_SEPARATOR);
            assert!(
                entries().iter().any(|e| menu_path(e.id) == Some(shown.as_str())),
                "{shown} has no palette entry"
            );
        }
        // One registry: unique ids and unique titles.
        let mut ids = std::collections::HashSet::new();
        let mut titles = std::collections::HashSet::new();
        for e in entries() {
            assert!(ids.insert(e.id), "duplicate id {}", e.id);
            assert!(
                titles.insert(format!("{}: {}", e.category, e.title)),
                "duplicate title {}",
                e.title
            );
        }
        let open = find("menu:File/Open level…").unwrap();
        assert_eq!((open.category, open.title), ("File", "Open level…"));
        let row = row_label(&find("edit.undo").unwrap(), "Ctrl+Z", true);
        assert_eq!(row, "★ Edit: Undo    Edit ▸ Undo    Ctrl+Z");
        let mut p = CommandPalette::default();
        assert_eq!(
            p.rank("")[0].def.id,
            "search.everything",
            "the first row with an empty query"
        );
        p.note_run("view.toggle_grid");
        assert!(
            p.rank("open level")
                .iter()
                .any(|r| r.def.id == "menu:File/Open level…")
        );
    }

    #[test]
    fn menu_entries_run_as_their_menu_item() {
        let mut world = World::new();
        let a = world.spawn_empty().id();
        let line = run(&mut world, "menu:Edit/Select all").unwrap();
        assert_eq!(line, "Edit ▸ Select all");
        assert!(world.resource::<Selection>().contains(a));
        assert_eq!(
            run(&mut world, "menu:File/Open level…").unwrap(),
            "File ▸ Open level… opens a dialog: choose it from the menu"
        );
        assert_eq!(
            run(&mut world, "menu:File/Package for platform…").unwrap(),
            "File ▸ Package for platform… opens a dialog: choose it from the menu",
            "a confirm-carrying call is not made from the palette"
        );
        assert_eq!(
            world.resource::<CommandPalette>().recent[0],
            "menu:File/Package for platform…"
        );
        // Commands that open windows.
        run(&mut world, "search.everything").unwrap();
        assert!(world.resource::<crate::universal_search::UniversalSearch>().open);
        run(&mut world, "view.favorites").unwrap();
        assert!(world.resource::<PaletteFavorites>().popup_open);
        run(&mut world, "selection.history").unwrap();
        assert!(world.resource::<PaletteFavorites>().history_open);
    }

    /// The new default chords collide with no built-in shortcut and no fixed editor key (P-030:
    /// Ctrl+Shift+F was picked after checking every `consume_shortcut` and shortcut in the crate).
    #[test]
    fn new_default_chords_have_no_conflict() {
        let new = [
            "search.everything",
            "view.favorites",
            "selection.back",
            "selection.forward",
        ];
        for id in new {
            let chord = KeyChord::parse(find(id).unwrap().shortcut).unwrap();
            for def in COMMANDS.iter().filter(|d| d.id != id && !d.shortcut.is_empty()) {
                assert_ne!(
                    KeyChord::parse(def.shortcut),
                    Some(chord),
                    "{id} shares {} with {}",
                    def.shortcut,
                    def.id
                );
            }
        }
        for (keys, what) in crate::editor_keymap::FIXED_EDITOR_KEYS {
            let fixed = KeyChord::parse(keys).unwrap();
            for id in [
                "search.everything",
                "view.favorites",
                "selection.back",
                "selection.forward",
            ] {
                let chord = KeyChord::parse(find(id).unwrap().shortcut).unwrap();
                assert_ne!(chord, fixed, "{id} collides with {what}");
            }
        }
        // A user binding of the new chord elsewhere is reported as shadowing it.
        let mut keymap = CommandKeymap::default();
        keymap
            .bind("view.toggle_grid", KeyChord::parse("Ctrl+T").unwrap())
            .unwrap();
        let conflicts = crate::editor_keymap::conflicts(&keymap);
        assert!(
            conflicts.iter().any(|c| c.shadows.contains("search.everything")),
            "{conflicts:?}"
        );
    }

    #[test]
    fn favorites_pin_from_the_agent_and_the_keyboard() {
        use crate::canvas_keys::test_input::{key, raw, text};
        let mut world = World::new();
        let out = editor_command(&mut world, &json!({ "pin": { "id": "view.toggle_grid" } })).unwrap();
        assert_eq!(out["favorites"], json!(["view.toggle_grid"]));
        assert_eq!(out["pinned"]["pinned"], true);
        let code = |e: RpcError| e.data.unwrap()["code"].as_str().unwrap().to_owned();
        assert_eq!(
            code(editor_command(&mut world, &json!({ "pin": { "id": "nope" } })).unwrap_err()),
            "unknown_command"
        );
        assert_eq!(
            code(
                editor_command(&mut world, &json!({ "pin": { "id": "edit.undo", "pinned": 1 } }))
                    .unwrap_err()
            ),
            "bad_pin"
        );
        let out = editor_command(&mut world, &json!({ "query": "realtime" })).unwrap();
        assert_eq!(out["commands"][0]["menu_path"], "View ▸ Realtime viewport");
        // Keyboard: Ctrl+Shift+P, type, F4 pins; Ctrl+Shift+F opens the popup; Down, Enter runs.
        let ctx = egui::Context::default();
        let frame = |world: &mut World, events: Vec<egui::Event>| {
            let mut status = None;
            let mut out = ctx.run_ui(raw([1200.0, 800.0], events), |ui| {
                status = show(ui.ctx(), world, false);
            });
            out.textures_delta.clear();
            status
        };
        frame(
            &mut world,
            vec![key(
                egui::Key::P,
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
            )],
        );
        frame(&mut world, vec![]);
        frame(&mut world, vec![text("toggle labels")]);
        frame(&mut world, vec![key(egui::Key::F4, egui::Modifiers::NONE)]);
        assert_eq!(
            world.resource::<PaletteFavorites>().ids,
            ["view.toggle_grid", "view.toggle_labels"]
        );
        frame(&mut world, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        frame(
            &mut world,
            vec![key(
                egui::Key::F,
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
            )],
        );
        assert!(
            world.resource::<PaletteFavorites>().popup_open,
            "Ctrl+Shift+F opens the favorites"
        );
        frame(&mut world, vec![key(egui::Key::ArrowDown, egui::Modifiers::NONE)]);
        let before = world
            .get_resource::<ViewportSettings>()
            .map_or(ViewportSettings::default().labels, |s| s.labels);
        let status = frame(&mut world, vec![key(egui::Key::Enter, egui::Modifiers::NONE)]);
        assert!(status.unwrap().starts_with("Selection labels"));
        assert_eq!(world.resource::<ViewportSettings>().labels, !before);
        assert!(!world.resource::<PaletteFavorites>().popup_open);
    }

    /// Lane v06-editor-undo-agent-routes (ADR-8583, smoke bug 9): Ctrl+Shift+P opens the palette
    /// whichever widget has the keyboard focus, a text field drawn before the palette's key
    /// handler (the menu bar's) or after it (a docked panel's) included. The chord arrives the way
    /// egui-winit sends it on Linux and Windows (Ctrl and Command both set).
    #[test]
    fn the_palette_chord_opens_from_a_focused_text_field() {
        use crate::canvas_keys::test_input::raw;
        let chord = egui::Event::Key {
            key: egui::Key::P,
            physical_key: Some(egui::Key::P),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers {
                ctrl: true,
                shift: true,
                command: true,
                ..egui::Modifiers::NONE
            },
        };
        for before in [true, false] {
            let mut world = World::new();
            world.insert_resource(crate::editor_keymap::FocusedPanel(Some(
                crate::shell::Panel::Outliner,
            )));
            let ctx = egui::Context::default();
            let mut field = String::from("rock");
            let id = egui::Id::new("smoke-text-field");
            let frame = |world: &mut World, field: &mut String, events: Vec<egui::Event>| {
                let mut out = ctx.run_ui(raw([1200.0, 800.0], events), |ui| {
                    let mut draw_field = |ui: &mut egui::Ui| {
                        ui.add(egui::TextEdit::singleline(field).id(id));
                    };
                    if before {
                        draw_field(ui);
                    }
                    show(ui.ctx(), world, false);
                    if !before {
                        draw_field(ui);
                    }
                });
                out.textures_delta.clear();
            };
            frame(&mut world, &mut field, Vec::new());
            ctx.memory_mut(|m| m.request_focus(id));
            frame(&mut world, &mut field, Vec::new());
            assert!(ctx.text_edit_focused(), "the text field has the focus");
            frame(&mut world, &mut field, vec![chord.clone()]);
            assert!(
                world.resource::<CommandPalette>().open,
                "Ctrl+Shift+P opened the palette (text field drawn {} the handler)",
                if before { "before" } else { "after" }
            );
            assert_eq!(field, "rock", "the chord typed nothing into the field");
            // Close it again; during an IME composition the chord is the input method's.
            frame(&mut world, &mut field, vec![chord.clone()]);
            assert!(
                !world.resource::<CommandPalette>().open,
                "the chord toggles it closed"
            );
            ctx.memory_mut(|m| m.request_focus(id));
            frame(&mut world, &mut field, Vec::new());
            let preedit = |text: &str| {
                egui::Event::Ime(egui::ImeEvent::Preedit {
                    text: text.to_owned(),
                    active_range_chars: None,
                })
            };
            frame(&mut world, &mut field, vec![preedit("ni")]);
            frame(&mut world, &mut field, vec![chord.clone()]);
            assert!(
                !world.resource::<CommandPalette>().open,
                "no palette during an IME composition"
            );
            frame(&mut world, &mut field, vec![preedit("")]);
            frame(&mut world, &mut field, vec![chord.clone()]);
            assert!(
                world.resource::<CommandPalette>().open,
                "the chord works again once the composition ends"
            );
        }
    }
}
