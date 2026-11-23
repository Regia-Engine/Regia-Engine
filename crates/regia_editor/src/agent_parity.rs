// SPDX-License-Identifier: MPL-2.0
//! Editor agent parity (ADR-7307): every button and menu item of the editor is reachable through
//! the agent protocol, and a test keeps it that way.
//!
//! Three pieces:
//!
//! - [`ACTIONS`], the action registry: one [`EditorAction`] per registered UI action (id, label,
//!   panel) with the agent path that performs it ([`ActionPath`]: an agent method, an
//!   `editor.command` palette id, or a class for actions that change no state an agent needs).
//!   UI code draws a registered action with [`action_button`], [`action_menu_item`],
//!   [`action_button_text`], [`action_enabled`] or [`action_toggle`], so the label and the agent
//!   path cannot drift apart.
//! - The allowlist `crates/regia_editor/agent-parity-allow.ron` ([`allowlist`]): every other
//!   `.button(`, `.small_button(`, `menu_button(`, `selectable_label(`, `.checkbox(`, `Button::new(`,
//!   `ImageButton::new(`, `radio_value(`, `toggle_value(`, `glyph_button(` and `bare_checkbox(` site
//!   of non-test editor code, classified (see [`SiteClass`]). A site that changes state and has no
//!   agent path yet is class `gap`; the scan test fails on a `gap` row its `KNOWN_GAPS` list does not
//!   name (ADR-7452).
//! - Value editors (ADR-7747): every `DragValue::new(`, `ComboBox::`, `selectable_value(`,
//!   `Slider::new(`, `TextEdit::singleline(`/`multiline(`, `text_edit_singleline(`/`multiline(`,
//!   `drag_angle(` and `color_edit_button` site is classified the same way, in the allowlist's last
//!   section ([`is_value_call`]). A value-editor row of class `view_only` or `gap` carries a `note`:
//!   why the value needs no agent path, or what is missing and which follow-up adds it. The
//!   reflect-driven Inspector widgets are `method` rows via `editor.set_property`, the generic
//!   route that writes the same field.
//! - The source scan ([`scan_sites`], [`parity_report`]), run by the `agent_parity_scan`
//!   integration test: it fails on a site that is neither drawn through a helper nor listed, and on
//!   a listed site that no longer exists. `REGIA_AGENT_PARITY_UPDATE=1 cargo test -p regia_editor
//!   --test agent_parity_scan` rewrites the allowlist (known sites keep their class, new ones are
//!   added as `gap`), which is how the lead registers another lane's new UI at integration.
//!
//! The agent surface is `editor.actions { panel?, class? }` (`book/agents/editor-command.md`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// How an agent performs a UI action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionPath {
    /// The agent method that does what the control does.
    Method(&'static str),
    /// The `editor.command` palette id that does what the control does
    /// (`editor.command { run: "<id>" }`).
    Command(&'static str),
    /// Scroll, collapse, hover, tab or layout only: no state an agent needs.
    ViewOnly,
    /// The control opens an OS file dialog; the agent calls this method with the path instead.
    OsDialog(&'static str),
    /// A confirmation dialog; the agent states its choice as this method's parameters.
    Confirm(&'static str),
}

impl ActionPath {
    /// The class name the allowlist and `editor.actions` use: `method`, `command`, `view_only`,
    /// `os_dialog` or `confirm`.
    #[must_use]
    pub fn class(self) -> &'static str {
        match self {
            Self::Method(_) => "method",
            Self::Command(_) => "command",
            Self::ViewOnly => "view_only",
            Self::OsDialog(_) => "os_dialog",
            Self::Confirm(_) => "confirm",
        }
    }

    /// The method or command id, when the path names one.
    #[must_use]
    pub fn via(self) -> Option<&'static str> {
        match self {
            Self::Method(id) | Self::Command(id) | Self::OsDialog(id) | Self::Confirm(id) => Some(id),
            Self::ViewOnly => None,
        }
    }
}

/// One registered UI action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorAction {
    /// Stable id (`panel.action`).
    pub id: &'static str,
    /// The label the control shows (helpers may decorate it, see [`action_button_text`]).
    pub label: &'static str,
    /// The panel or menu the control lives in (`shell.file`, `shell.play`, ...).
    pub panel: &'static str,
    /// What an agent calls instead of clicking.
    pub path: ActionPath,
}

macro_rules! actions {
    ($($(#[$doc:meta])* $name:ident = ($id:literal, $label:literal, $panel:literal, $path:expr);)*) => {
        $( $(#[$doc])* pub const $name: EditorAction = EditorAction {
            id: $id,
            label: $label,
            panel: $panel,
            path: $path,
        }; )*
        /// Every registered UI action, in menu order.
        pub const ACTIONS: &[EditorAction] = &[$($name),*];
    };
}

use ActionPath::{Command, Confirm, Method, OsDialog};

actions! {
    /// File → New level.
    FILE_NEW_LEVEL = ("file.new_level", "New level", "shell.file", Command("file.new_level"));
    /// File → Open level….
    FILE_OPEN_LEVEL = ("file.open_level", "Open level…", "shell.file", OsDialog("editor.open_level"));
    /// File → Open kit… (and the project browser's button).
    FILE_OPEN_KIT = ("file.open_kit", "Open kit…", "shell.file", OsDialog("editor.open_kit"));
    /// File → Save level.
    FILE_SAVE_LEVEL = ("file.save_level", "Save level", "shell.file", Method("editor.save_level"));
    /// File → Save level as….
    FILE_SAVE_LEVEL_AS = ("file.save_level_as", "Save level as…", "shell.file", OsDialog("editor.save_level"));
    /// File → Export selection to glTF….
    FILE_EXPORT_GLB = ("file.export_glb", "Export selection to glTF…", "shell.file", OsDialog("asset.export_glb"));
    /// File → Create entity.
    FILE_CREATE_ENTITY = ("file.create_entity", "Create entity", "shell.file", Method("editor.spawn"));
    /// Create → Shape → (one per kind): `editor.command { run: "create.<kind>" }`.
    CREATE_SHAPE = ("create.shape", "Shape", "shell.create", Command("create.cuboid"));
    /// Edit → Undo.
    EDIT_UNDO = ("edit.undo", "Undo", "shell.edit", Command("edit.undo"));
    /// Edit → Redo.
    EDIT_REDO = ("edit.redo", "Redo", "shell.edit", Command("edit.redo"));
    /// Edit → Preferences….
    EDIT_PREFERENCES = ("edit.preferences", "Preferences…", "shell.edit", Command("window.preferences"));
    /// Edit → Project settings….
    EDIT_PROJECT_SETTINGS = ("edit.project_settings", "Project settings…", "shell.edit", Command("window.project_settings"));
    /// Edit → World settings….
    EDIT_WORLD_SETTINGS = ("edit.world_settings", "World settings…", "shell.edit", Command("window.world_settings"));
    /// View → Grid.
    VIEW_GRID = ("view.grid", "Grid", "shell.view", Command("view.toggle_grid"));
    /// View → Selection labels.
    VIEW_LABELS = ("view.labels", "Selection labels", "shell.view", Command("view.toggle_labels"));
    /// View → Realtime viewport.
    VIEW_REALTIME = ("view.realtime", "Realtime viewport", "shell.view", Command("view.toggle_realtime"));
    /// View → Profiler.
    VIEW_PROFILER = ("view.profiler", "Profiler", "shell.view", Command("view.toggle_profiler"));
    /// Open the frame debugger.
    VIEW_FRAME_DEBUGGER = ("view.frame_debugger", "Frame Debugger", "shell.view", Command("view.toggle_frame_debugger"));
    /// Capture the next viewport frame.
    FRAME_CAPTURE = ("frame_debugger.capture", "Capture frame", "frame_debugger", Method("editor.frame_capture"));
    /// Resume live viewport rendering.
    FRAME_RESUME = ("frame_debugger.resume", "Resume", "frame_debugger", Method("editor.frame_capture"));
    /// View → Diagnostics.
    VIEW_DIAGNOSTICS = ("view.diagnostics", "Diagnostics", "shell.view", Command("view.toggle_diagnostics"));
    /// View → Frame selection.
    VIEW_FRAME_SELECTION = ("view.frame_selection", "Frame selection (F)", "shell.view", Command("view.frame_selection"));
    /// View → Reset zoom (ADR-7733: egui's Ctrl+0 zoom reset moved here; Ctrl+0 is camera bookmark 0).
    VIEW_RESET_ZOOM = ("view.reset_zoom", "Reset zoom", "shell.view", Command("view.reset_zoom"));
    /// View → Save layout to project.
    VIEW_LAYOUT_SAVE = ("view.layout_save", "Save layout to project", "shell.view", Command("layout.save"));
    /// View → Save layout as my default.
    VIEW_LAYOUT_SAVE_DEFAULT = ("view.layout_save_default", "Save layout as my default", "shell.view", Command("layout.save_default"));
    /// View → Reset layout.
    VIEW_LAYOUT_RESET = ("view.layout_reset", "Reset layout", "shell.view", Command("layout.reset"));
    /// Play toolbar → Play.
    PLAY_PLAY = ("play.play", "▶ Play", "shell.play", Method("editor.play"));
    /// Play toolbar → Pause.
    PLAY_PAUSE = ("play.pause", "⏸ Pause", "shell.play", Method("editor.pause"));
    /// Play toolbar → Resume (in place of Pause while paused, ADR-9810 E-1).
    PLAY_RESUME = ("play.resume", "▶ Resume", "shell.play", Method("editor.resume"));
    /// Play toolbar → Stop.
    PLAY_STOP = ("play.stop", "⏹ Stop", "shell.play", Method("editor.stop"));
    /// Play toolbar → Step (while paused).
    PLAY_STEP = ("play.step", "⏭ Step", "shell.play", Method("editor.step"));
    /// Play toolbar → Compile (rebuild and hot-reload the game code).
    PLAY_COMPILE = ("play.compile", "⚙ Compile", "shell.play", Method("agent.hot_reload"));
    /// Prefab banner → Save.
    PREFAB_SAVE = ("prefab.save", "Save", "shell.prefab", Method("editor.save_prefab"));
    /// Prefab banner → Save & Close.
    PREFAB_SAVE_CLOSE = ("prefab.save_close", "Save & Close", "shell.prefab", Method("editor.close_prefab"));
    /// Prefab banner → Discard & Close (`editor.close_prefab { save: false }`).
    PREFAB_DISCARD_CLOSE = ("prefab.discard_close", "Discard & Close", "shell.prefab", Method("editor.close_prefab"));
    /// Crash recovery window → Restore (the first pending autosave becomes the open level).
    RECOVERY_RESTORE = ("recovery.restore", "Restore", "shell.recovery", Command("recovery.restore"));
    /// Crash recovery window → Discard autosave.
    RECOVERY_DISCARD = ("recovery.discard", "Discard autosave", "shell.recovery", Command("recovery.discard"));
    /// Viewport toolbar → SpaceMouse moves selection (the SpaceMouse Object tool, specs/29 § 2.3b).
    VIEWPORT_SPACEMOUSE_OBJECT = ("viewport.spacemouse_object", "SpaceMouse moves selection", "viewport.toolbar", Command("viewport.toggle_spacemouse_object"));
    /// Material panel → Undo (the panel's own history).
    MATERIAL_UNDO = ("material.undo", "Undo", "material_editor", Command("material.undo"));
    /// Material panel → Redo.
    MATERIAL_REDO = ("material.redo", "Redo", "material_editor", Command("material.redo"));
    /// Material panel → Reload from disk (drops the panel's unsaved edits).
    MATERIAL_RELOAD = ("material.reload", "Reload from disk", "material_editor", Command("material.reload"));
    /// Terrain panel → Level terrain → Retry (rebuild after a failed load).
    TERRAIN_RETRY = ("terrain.retry_level_terrain", "Retry", "terrain_panel", Command("terrain.retry_level_terrain"));
    /// Shots panel → Open (the path row's `.shots.ron`, ADR-8730).
    SHOTS_OPEN = ("shots.open", "Open", "shot_panel", Method("editor.shot_panel_open"));
    /// Shots panel → Save (also Ctrl+S over the panel, ADR-8730).
    SHOTS_SAVE = ("shots.save", "Save", "shot_panel", Method("editor.shot_panel_save"));
    /// Shots panel → Edit player's shots (a sequence player's own list as the document, ADR-8940).
    SHOTS_ADOPT = ("shots.adopt", "Edit player's shots", "shot_panel", Method("editor.shot_panel_adopt"));
    /// Unsaved-changes prompt → Save and exit (an agent saves with `editor.save_level`, then
    /// `agent.shutdown`).
    CLOSE_SAVE_EXIT = ("close.save_and_exit", "Save and exit", "shell.close", Confirm("agent.shutdown"));
    /// Unsaved-changes prompt → Discard and exit.
    CLOSE_DISCARD_EXIT = ("close.discard_and_exit", "Discard and exit", "shell.close", Confirm("agent.shutdown"));
    /// Unsaved-changes prompt → Cancel.
    CLOSE_CANCEL = ("close.cancel", "Cancel", "shell.close", ActionPath::ViewOnly);
    /// Content Browser row menu → Import settings… (ADR-7469).
    CONTENT_IMPORT_SETTINGS = ("content.import_settings", "Import settings…", "content_browser", Method("editor.import_settings_open"));
    /// Import settings panel → Apply (write the `.meta` and cook once).
    IMPORT_SETTINGS_APPLY = ("import_settings.apply", "Apply", "import_settings", Method("editor.import_settings_apply"));
    /// Import settings panel → Revert (drop the unsaved edits).
    IMPORT_SETTINGS_REVERT = ("import_settings.revert", "Revert", "import_settings", Method("editor.import_settings_revert"));
    /// Import settings panel → a field's Reset button (the importer default in every edited source; ADR-7744).
    IMPORT_SETTINGS_FIELD_RESET = ("import_settings.field_reset", "Reset", "import_settings", Method("editor.import_settings_edit"));
    /// Asset viewer → UV0 overlay (`editor.view_asset { overlay: "uv" }`).
    VIEWER_OVERLAY_UV = ("asset_viewer.overlay_uv", "UV0", "asset_viewer", Method("editor.view_asset"));
    /// Asset viewer → UV1 overlay (`editor.view_asset { overlay: "uv1" }`).
    VIEWER_OVERLAY_UV1 = ("asset_viewer.overlay_uv1", "UV1", "asset_viewer", Method("editor.view_asset"));
    /// Asset viewer → vertex normals overlay (`editor.view_asset { overlay: "normals" }`).
    VIEWER_OVERLAY_NORMALS = ("asset_viewer.overlay_normals", "Normals", "asset_viewer", Method("editor.view_asset"));
    /// Asset viewer → collision overlay (`editor.view_asset { overlay: "collision" }`).
    VIEWER_OVERLAY_COLLISION = ("asset_viewer.overlay_collision", "Collision", "asset_viewer", Method("editor.view_asset"));
    /// Outliner row menu → Create prefab from selection… (opens the file dialog; ADR-7468).
    OUTLINER_CREATE_PREFAB = ("outliner.create_prefab", "Create prefab from selection…", "outliner", Method("editor.create_prefab"));
    /// Create prefab dialog → Create.
    OUTLINER_CREATE_PREFAB_CONFIRM = ("outliner.create_prefab_confirm", "Create prefab", "outliner.create_prefab", Method("editor.create_prefab"));
    /// Create prefab dialog → Cancel.
    OUTLINER_CREATE_PREFAB_CANCEL = ("outliner.create_prefab_cancel", "Cancel", "outliner.create_prefab", ActionPath::ViewOnly);
    /// Create → Player start (`editor.create_entity { kind: "player_start" }`; ADR-7468).
    CREATE_PLAYER_START = ("create.player_start", "Player start", "shell.create", Method("editor.create_entity"));
    /// Create → Trigger volume (a sensor box collider).
    CREATE_TRIGGER_VOLUME = ("create.trigger_volume", "Trigger volume", "shell.create", Method("editor.create_entity"));
    /// Create → Point light.
    CREATE_POINT_LIGHT = ("create.point_light", "Point light", "shell.create", Method("editor.create_entity"));
    /// Create → Spot light.
    CREATE_SPOT_LIGHT = ("create.spot_light", "Spot light", "shell.create", Method("editor.create_entity"));
    /// Create → Directional light.
    CREATE_DIRECTIONAL_LIGHT = ("create.directional_light", "Directional light", "shell.create", Method("editor.create_entity"));
    /// Create → Camera.
    CREATE_CAMERA = ("create.camera", "Camera", "shell.create", Method("editor.create_entity"));
    /// Viewport toolbar → Show → Navigation (`editor.set_viewport { navigation }`).
    VIEW_SHOW_NAVIGATION = ("viewport.show_navigation", "Navigation", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Resolution (the resolution preview frame; `editor.viewport_resolution_preview`, ADR-7744).
    VIEW_RESOLUTION_PREVIEW = ("viewport.resolution_preview", "Resolution", "viewport.toolbar", Method("editor.viewport_resolution_preview"));
    /// File → New script… (the New script wizard, `editor.script_wizard { open: true }`; ADR-7475).
    FILE_NEW_SCRIPT = ("file.new_script", "New script…", "shell.file", Method("editor.script_wizard"));
    /// File → Save all: opens the Save all dialog (`editor.save_all { dialog: true }`, ADR-7822).
    FILE_SAVE_ALL = ("file.save_all", "Save all", "shell.file", Method("editor.save_all"));
    /// File → Recent (`editor.menu_invoke { path: "File/Recent/<n>" }`).
    FILE_RECENT = ("file.recent", "Recent", "shell.file", Method("editor.menu_invoke"));
    /// File → Package for platform… (the dialog; `editor.package_start`).
    FILE_PACKAGE = ("file.package", "Package for platform…", "shell.file", Method("editor.package_start"));
    /// File → Exit (`editor.menu_invoke { path: "File/Exit" }`).
    FILE_EXIT = ("file.exit", "Exit", "shell.file", Method("editor.menu_invoke"));
    /// Edit → Cut (`editor.menu_invoke { path: "Edit/Cut" }`).
    EDIT_CUT = ("edit.cut", "Cut", "shell.edit", Method("editor.menu_invoke"));
    /// Edit → Copy.
    EDIT_COPY = ("edit.copy", "Copy", "shell.edit", Method("editor.menu_invoke"));
    /// Edit → Paste.
    EDIT_PASTE = ("edit.paste", "Paste", "shell.edit", Method("editor.menu_invoke"));
    /// Edit → Duplicate.
    EDIT_DUPLICATE = ("edit.duplicate", "Duplicate (Ctrl+D)", "shell.edit", Method("editor.menu_invoke"));
    /// Edit → Delete.
    EDIT_DELETE = ("edit.delete", "Delete", "shell.edit", Method("editor.menu_invoke"));
    /// Edit → Select all.
    EDIT_SELECT_ALL = ("edit.select_all", "Select all", "shell.edit", Method("editor.menu_invoke"));
    /// Edit → Paste as child (`editor.paste { into }`, or `editor.menu_invoke { path: "Edit/Paste as child" }`).
    EDIT_PASTE_AS_CHILD = ("edit.paste_as_child", "Paste as child", "shell.edit", Method("editor.menu_invoke"));
    /// Edit → Paste at original position (`editor.paste { at_original: true }`).
    EDIT_PASTE_AT_ORIGINAL = ("edit.paste_at_original", "Paste at original position", "shell.edit", Method("editor.menu_invoke"));
    /// Tools → Rebuild asset database.
    TOOLS_REBUILD_ASSET_DB = ("tools.rebuild_asset_db", "Rebuild asset database", "shell.tools", Method("editor.rebuild_asset_db"));
    /// Tools → Open code editor (spec 19 § 5; `editor.open_code_editor`).
    TOOLS_OPEN_CODE_EDITOR = ("tools.open_code_editor", "Open code editor", "shell.tools", Method("editor.open_code_editor"));
    /// Tools → Clear caches… (opens the confirm window; `editor.clear_caches { confirm: true }`).
    TOOLS_CLEAR_CACHES = ("tools.clear_caches", "Clear caches…", "shell.tools", Confirm("editor.clear_caches"));
    /// Clear caches confirm window → Clear caches.
    TOOLS_CLEAR_CACHES_CONFIRM = ("tools.clear_caches_confirm", "Clear caches", "shell.tools", Confirm("editor.clear_caches"));
    /// Clear caches confirm window → Cancel.
    TOOLS_CLEAR_CACHES_CANCEL = ("tools.clear_caches_cancel", "Cancel", "shell.tools", ActionPath::ViewOnly);
    /// Play toolbar and menu → Simulate (`editor.play { mode: "simulate" }`).
    PLAY_SIMULATE = ("play.simulate", "▶ Simulate", "shell.play", Method("editor.play"));
    /// Play → Players → n players (`editor.play { players: n }`).
    PLAY_PLAYERS = ("play.players", "Players", "shell.play", Method("editor.play"));
    /// Help → Documentation (`editor.help_open { topic: "documentation" }`).
    HELP_DOCUMENTATION = ("help.documentation", "Documentation", "shell.help", Method("editor.help_open"));
    /// Help → Agent guide (`editor.help_open { topic: "agent_guide" }`).
    HELP_AGENT_GUIDE = ("help.agent_guide", "Agent guide", "shell.help", Method("editor.help_open"));
    /// Help → About Regia Engine (`editor.about { open: true }`).
    HELP_ABOUT = ("help.about", "About Regia Engine", "shell.help", Method("editor.about"));
    /// About window → License (opens LICENSE in the OS viewer; `editor.about` answers its path).
    HELP_ABOUT_LICENSE = ("help.about_license", "License", "shell.help", ActionPath::ViewOnly);
    /// About window → Third-party notices (opens THIRD_PARTY_NOTICES.md; `editor.about` answers its path).
    HELP_ABOUT_NOTICES = ("help.about_notices", "Third-party notices", "shell.help", ActionPath::ViewOnly);
    /// Package for platform window → Package.
    PACKAGE_START = ("package.start", "Package", "package_dialog", Method("editor.package_start"));
    /// Package for platform window → Open log (the log path is `editor.package_status`'s `log`).
    PACKAGE_OPEN_LOG = ("package.open_log", "Open log", "package_dialog", ActionPath::ViewOnly);
    /// Package for platform window → Show in folder (the artifact is `editor.package_status`'s).
    PACKAGE_SHOW_OUTPUT = ("package.show_output", "Show in folder", "package_dialog", ActionPath::ViewOnly);
    /// Project Settings → Icon → Choose icon… (`editor.project_icon { path }`).
    PROJECT_ICON_CHOOSE = ("project_settings.icon_choose", "Choose icon…", "project_settings", OsDialog("editor.project_icon"));
    /// Icon pick confirm → Replace icon (`editor.project_icon { path }`; overwrites the current icon).
    ICON_PICK_REPLACE = ("project_settings.icon_replace", "Replace icon", "project_settings", Method("editor.project_icon"));
    /// Icon pick confirm → Keep current (the existing icon stays).
    ICON_PICK_KEEP = ("project_settings.icon_keep", "Keep current", "project_settings", ActionPath::ViewOnly);
    /// Inspector → a component header's context menu and body → `Remove <component>` (ADR-7472).
    INSPECTOR_REMOVE_COMPONENT = ("inspector.remove_component", "Remove component", "inspector", Method("editor.remove_component"));
    /// Abilities inspector → asset editor → Advanced: the raw RON text instead of the typed form (ADR-7472).
    ABILITIES_ASSET_ADVANCED = ("abilities.asset_advanced", "Advanced (raw text)", "abilities_panel", ActionPath::ViewOnly);
    /// Output Log launch bar → Launch standalone (ADR-8980).
    LAUNCH_STANDALONE = ("launch.standalone", "Launch standalone", "launch_bar", Method("editor.launch"));
    /// Output Log launch bar → Launch on device (`editor.launch { target: "android" }`).
    LAUNCH_ANDROID = ("launch.android", "Launch on device", "launch_bar", Method("editor.launch"));
    /// Output Log launch bar → Stop the launch.
    LAUNCH_STOP = ("launch.stop", "Stop launch", "launch_bar", Method("editor.launch_stop"));
    /// Output Log launch bar → Refresh devices (`adb devices -l`).
    LAUNCH_REFRESH_DEVICES = ("launch.refresh_devices", "Refresh devices", "launch_bar", Method("editor.launch_devices"));
    /// Output Log launch bar (Android) → Device screen → Start mirror (ADR-7836).
    MIRROR_START = ("launch.mirror_start", "Start mirror", "launch_bar", Method("editor.device_mirror"));
    /// Output Log launch bar (Android) → Device screen → Stop mirror (ADR-7836).
    MIRROR_STOP = ("launch.mirror_stop", "Stop mirror", "launch_bar", Method("editor.device_mirror"));
    /// Output Log launch bar (Android) → Device screen → Send tap at the X and Y fields (ADR-7836).
    MIRROR_TAP = ("launch.mirror_tap", "Send tap", "launch_bar", Method("editor.device_mirror"));
    /// Viewport toolbar → Show → Colliders (`editor.set_viewport { colliders }`, ADR-7593).
    VIEW_SHOW_COLLIDERS = ("viewport.show_colliders", "Colliders", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Show → Light radii (`editor.set_viewport { light_radii }`, ADR-7593).
    VIEW_SHOW_LIGHT_RADII = ("viewport.show_light_radii", "Light radii", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Show → Post volumes (`editor.set_viewport { post_volumes }`, ADR-7593).
    VIEW_SHOW_POST_VOLUMES = ("viewport.show_post_volumes", "Post volumes", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Show → Build navigation preview (`editor.nav_preview { action: "build" }`, ADR-7593).
    VIEW_NAV_PREVIEW_BUILD = ("viewport.nav_preview_build", "Build navigation preview", "viewport.toolbar", Method("editor.nav_preview"));
    /// Viewport toolbar → Show → Clear navigation preview (`editor.nav_preview { action: "clear" }`, ADR-7593).
    VIEW_NAV_PREVIEW_CLEAR = ("viewport.nav_preview_clear", "Clear navigation preview", "viewport.toolbar", Method("editor.nav_preview"));
    /// Layers panel → Layer definitions → Create layer (ADR-7614).
    LAYERS_CREATE = ("layers.create_layer", "Create layer", "layers_panel", Method("editor.layer_create"));
    /// Layers panel → Layer definitions → Editor only (the new layer's kind).
    LAYERS_NEW_EDITOR_ONLY = ("layers.new_editor_only", "Editor only", "layers_panel", Method("editor.layer_create"));
    /// Layers panel → Layer definitions → Bind (a definition file to the open level).
    LAYERS_BIND = ("layers.bind", "Bind", "layers_panel", Method("editor.layer_bind"));
    /// Layers panel → Layer definitions → Unbind.
    LAYERS_UNBIND = ("layers.unbind", "Unbind", "layers_panel", Method("editor.layer_bind"));
    /// Layers panel → Layer definitions → Refresh (rescans assets/ for .layers.ron files).
    LAYERS_REFRESH_DEFS = ("layers.refresh_defs", "Refresh", "layers_panel", ActionPath::ViewOnly);
    /// Layers panel → a layer row's initial state (the saved state the level starts in).
    LAYERS_INITIAL = ("layers.initial", "Initial state", "layers_panel", Method("editor.layer_initial"));
    /// Layers panel → a layer row → Delete layer.
    LAYERS_DELETE = ("layers.delete_layer", "Delete layer", "layers_panel", Method("editor.layer_delete"));
    /// Layers panel → the selected placed level → Edit placed level.
    LAYERS_EDIT_PLACED = ("layers.edit_placed", "Edit placed level", "layers_panel", Method("editor.placed_edit"));
    /// Layers panel → the selected placed level → Break placed level (one undo step).
    LAYERS_BREAK_PLACED = ("layers.break_placed", "Break placed level", "layers_panel", Method("editor.break_placed"));
    /// Outliner row menu → Edit placed level.
    OUTLINER_EDIT_PLACED = ("outliner.edit_placed", "Edit placed level", "outliner", Method("editor.placed_edit"));
    /// Outliner row menu → Break placed level.
    OUTLINER_BREAK_PLACED = ("outliner.break_placed", "Break placed level", "outliner", Method("editor.break_placed"));
    /// Outliner row menu → Create variant… (a prefab instance's overrides become a variant).
    OUTLINER_CREATE_VARIANT = ("outliner.create_variant", "Create variant…", "outliner", Method("editor.create_prefab_variant"));
    /// Content Browser → a prefab's menu → Create variant… (an empty variant of it).
    CONTENT_CREATE_VARIANT = ("content.create_variant", "Create variant…", "content_browser", Method("editor.create_prefab_variant"));
    /// Create variant dialog → Create.
    VARIANT_CREATE_CONFIRM = ("prefab_variant.create_confirm", "Create variant", "prefab_variant_dialog", Method("editor.create_prefab_variant"));
    /// Create variant dialog → Cancel.
    VARIANT_CREATE_CANCEL = ("prefab_variant.create_cancel", "Cancel", "prefab_variant_dialog", ActionPath::ViewOnly);
    /// Inspector → the override marker beside a field → Revert override (`editor.prefab_override { action: "revert" }`; ADR-7744).
    INSPECTOR_OVERRIDE_REVERT = ("inspector.override_revert", "Revert override", "inspector", Method("editor.prefab_override"));
    /// Inspector → the override marker beside a field → Apply to prefab (`editor.prefab_override { action: "apply" }`; ADR-7744).
    INSPECTOR_OVERRIDE_APPLY = ("inspector.override_apply", "Apply to prefab", "inspector", Method("editor.prefab_override"));
    /// Inspector → a variant instance's overridden field → Clear override.
    INSPECTOR_CLEAR_VARIANT_OVERRIDE = ("inspector.clear_variant_override", "Clear override", "inspector", Method("editor.set_variant_override"));
    /// Create → Reflection capture (a `LocalCapture` box at the focus point; ADR-7592).
    CREATE_REFLECTION_CAPTURE = ("create.reflection_capture", "Reflection capture", "shell.create", Method("editor.create_entity"));
    /// Create → Audio emitter (a default `AudioEmitter`; w11-create-menu-breadth, ADR-7741).
    CREATE_AUDIO_EMITTER = ("create.audio_emitter", "Audio emitter", "shell.create", Method("editor.create_entity"));
    /// Create → Rect light (a default `RectLight`, aimed at the floor).
    CREATE_RECT_LIGHT = ("create.rect_light", "Rect light", "shell.create", Method("editor.create_entity"));
    /// Create → Tube light (a default `TubeLight`).
    CREATE_TUBE_LIGHT = ("create.tube_light", "Tube light", "shell.create", Method("editor.create_entity"));
    /// Create → Decal (a `StaticDecal` naming the built-in `scorch` prefab).
    CREATE_DECAL = ("create.decal", "Decal", "shell.create", Method("editor.create_entity"));
    /// Create → Post-process volume (a default `PostVolume`: a 2 m box with no overrides).
    CREATE_POST_PROCESS_VOLUME = ("create.post_process_volume", "Post-process volume", "shell.create", Method("editor.create_entity"));
    /// Create → Spline (a `Spline` of two points, 4 m apart along local X).
    CREATE_SPLINE = ("create.spline", "Spline", "shell.create", Method("editor.create_entity"));
    /// Create → Nav link (a default `NavLinkAnchor`).
    CREATE_NAV_LINK = ("create.nav_link", "Nav link", "shell.create", Method("editor.create_entity"));
    /// Create → Smart object (a default `SmartObject` with no definition).
    CREATE_SMART_OBJECT = ("create.smart_object", "Smart object", "shell.create", Method("editor.create_entity"));
    /// Tools → Bake reflection captures (a cancellable editor job writing the level's `.fcap`; ADR-7592).
    TOOLS_BAKE_CAPTURES = ("tools.bake_captures", "Bake reflection captures", "shell.tools", Method("editor.bake_captures"));
    /// The Bake reflection captures progress row's Cancel (`editor.cancel_task { id: "bake_captures" }`).
    TOOLS_BAKE_CAPTURES_CANCEL = ("tools.bake_captures_cancel", "Cancel", "capture_bake", Method("editor.cancel_task"));
    /// Import settings → the running Apply's Cancel (`editor.cancel_job { id: "import_settings:<n>" }`, ADR-7664).
    IMPORT_SETTINGS_APPLY_CANCEL = ("import_settings.apply_cancel", "Cancel", "import_settings", Method("editor.cancel_job"));
    /// Viewport toolbar → Show → Cancel navigation preview, while a bake runs (`editor.nav_preview { action: "cancel" }`, ADR-7664).
    VIEW_NAV_PREVIEW_CANCEL = ("viewport.nav_preview_cancel", "Cancel navigation preview", "viewport.toolbar", Method("editor.nav_preview"));
    /// Content Browser → a folder's menu → Validate folder (`editor.problems_panel { run: true, folder }`, ADR-7725).
    CONTENT_FOLDER_VALIDATE = ("content.folder_validate", "Validate folder", "content_browser", Method("editor.problems_panel"));
    /// Content Browser → a folder's menu → Reference graph (`editor.reference_graph_panel { action: "open", folder }`, ADR-7725).
    CONTENT_FOLDER_REFERENCE_GRAPH = ("content.folder_reference_graph", "Reference graph", "content_browser", Method("editor.reference_graph_panel"));
    /// Content Browser → a folder's menu → Size map (`editor.size_map_panel { folder }`, ADR-7725).
    CONTENT_FOLDER_SIZE_MAP = ("content.folder_size_map", "Size map", "content_browser", Method("editor.size_map_panel"));
    /// Content Browser → a folder's menu → Audit to CSV (`editor.asset_audit { source: "registry", folder }`, ADR-7725).
    CONTENT_FOLDER_AUDIT = ("content.folder_audit", "Audit to CSV", "content_browser", Method("editor.asset_audit"));
    /// Sequence → Render movie… (the dialog; ADR-7726).
    SEQUENCE_RENDER_MOVIE = ("sequence.render_movie", "Render movie…", "shell.sequence", Method("editor.render_movie"));
    /// Render movie dialog → Render (a cancellable editor job running `render.out_job`).
    RENDER_MOVIE_START = ("render_movie.start", "Render", "render_movie_dialog", Method("editor.render_movie"));
    /// Render movie dialog → Cancel, while it renders (`editor.cancel_task { id: "render_movie" }`).
    RENDER_MOVIE_CANCEL = ("render_movie.cancel", "Cancel", "render_movie_dialog", Method("editor.cancel_task"));
    /// Sequence Curves panel → Restore on stop (`.seq.ron` `restore_on_stop`, one undo step).
    SEQ_RESTORE_ON_STOP = ("seq.restore_on_stop", "Restore on stop", "seq_curve_panel", Method("editor.seq_restore_on_stop"));
    /// Shots panel → position or rotation track → Add key at playhead.
    SHOTS_POSE_KEY_ADD = ("shots.pose_key_add", "Add key at playhead", "shot_panel", Method("editor.shot_panel_edit"));
    /// Shots panel → position or rotation track → Delete key.
    SHOTS_POSE_KEY_DELETE = ("shots.pose_key_delete", "Delete key", "shot_panel", Method("editor.shot_panel_edit"));
    /// Abilities inspector → asset editor → Undo (`editor.abilities_panel_asset { undo: true }`).
    ABILITIES_ASSET_UNDO = ("abilities.asset_undo", "Undo", "abilities_panel", Method("editor.abilities_panel_asset"));
    /// Abilities inspector → asset editor → Redo (`editor.abilities_panel_asset { redo: true }`).
    ABILITIES_ASSET_REDO = ("abilities.asset_redo", "Redo", "abilities_panel", Method("editor.abilities_panel_asset"));
    /// Viewport toolbar → Tools → Surface snap while dragging (`editor.placement_options { surface_snap }`, ADR-7756).
    VIEW_SURFACE_SNAP = ("viewport.surface_snap", "Surface snap while dragging", "viewport.toolbar", Method("editor.placement_options"));
    /// Viewport toolbar → Tools → Align to surface while snapping (`editor.placement_options { surface_align }`, ADR-7756).
    VIEW_SURFACE_ALIGN = ("viewport.surface_align", "Align to surface while snapping", "viewport.toolbar", Method("editor.placement_options"));
    /// Inspector header → the lock toggle: stay on this entity while the selection changes (`editor.inspector_lock`).
    INSPECTOR_LOCK = ("inspector.lock", "\u{1f512} Lock", "inspector", Method("editor.inspector_lock"));
    /// Inspector header → Open pinned inspector: a floating Inspector locked to the entity (`editor.inspector_open_pinned`).
    INSPECTOR_OPEN_PINNED = ("inspector.open_pinned", "Open pinned inspector", "inspector", Method("editor.inspector_open_pinned"));
    /// Inspector header → Modified only: hide fields equal to their default (`editor.inspect_selection { modified_only }`).
    INSPECTOR_MODIFIED_ONLY = ("inspector.modified_only", "Modified only", "inspector", Method("editor.inspect_selection"));
    /// Inspector → a field label's context menu → Pin to top / Unpin from top (`editor.inspector_pin`).
    INSPECTOR_PIN_FIELD = ("inspector.pin_field", "Pin to top", "inspector", Method("editor.inspector_pin"));
    /// Inspector → Add component… and the picker's rows (`editor.add_component`; the list is `editor.component_types`).
    INSPECTOR_ADD_COMPONENT = ("inspector.add_component", "Add component…", "inspector", Method("editor.add_component"));
    /// View → Maximize panel (Ctrl+Space; ADR-7753).
    VIEW_MAXIMIZE_PANEL = ("view.maximize_panel", "Maximize panel (Ctrl+Space)", "shell.view", Command("view.maximize_panel"));
    /// View → Full-screen viewport (F11; ADR-7753).
    VIEW_FULLSCREEN_VIEWPORT = ("view.fullscreen_viewport", "Full-screen viewport (F11)", "shell.view", Command("view.fullscreen_viewport"));
    /// View → Workspace → (one per workspace), and the toolbar switcher (ADR-7753).
    VIEW_WORKSPACE = ("view.workspace", "Workspace", "shell.view", Method("editor.workspace_set"));
    /// View → Layouts → Save, a layout, Rename, Delete (ADR-7753).
    VIEW_NAMED_LAYOUTS = ("view.named_layouts", "Layouts", "shell.view", Method("editor.layout_save_named"));
    /// View → Panels → (one per panel) (ADR-7753).
    VIEW_PANELS = ("view.panels", "Panels", "shell.view", Method("editor.layout_apply"));
    /// View → Feature profile → (one per profile) (ADR-7753).
    VIEW_FEATURE_PROFILE = ("view.feature_profile", "Feature profile", "shell.view", Method("editor.feature_profile_set"));
    /// Status bar → Content: toggles the content drawer (ADR-7754).
    STATUS_CONTENT_DRAWER = ("status.content_drawer", "Content", "status_bar", Command("view.content_drawer"));
    /// Status bar → Log: toggles the log drawer with the console line (ADR-7754).
    STATUS_LOG_DRAWER = ("status.log_drawer", "Log", "status_bar", Command("view.log_drawer"));
    /// Status bar → Unsaved: N: File ▸ Save all (ADR-7754).
    STATUS_SAVE_ALL = ("status.save_all", "Unsaved", "status_bar", Method("editor.menu_invoke"));
    /// Status bar → Jobs: N → Cancel on a job's row (ADR-7754; `bake_gi` is `editor.bake_gi_cancel`).
    STATUS_JOB_CANCEL = ("status.job_cancel", "Cancel", "status_bar", Method("editor.cancel_task"));
    /// Status bar → Trace: starts or stops the trace recording (ADR-7754).
    STATUS_TRACE = ("status.trace", "Trace", "status_bar", Method("editor.trace_record"));
    /// Status bar → Problems: N: opens the Problems tab (ADR-7754).
    STATUS_PROBLEMS = ("status.problems", "Problems", "status_bar", Command("window.open_problems"));
    /// A drawer's Dock: closes it and opens its panel as a tab (ADR-7754).
    DRAWER_DOCK = ("drawer.dock", "Dock", "drawer", Method("editor.drawer"));
    /// A drawer's Close (ADR-7754).
    DRAWER_CLOSE = ("drawer.close", "Close", "drawer", Method("editor.drawer"));
    /// Main toolbar → Quick settings → Snap (the viewport toolbar's Snap, ADR-7754).
    QUICK_SNAP = ("quick.snap", "Snap", "quick_settings", Method("editor.set_snap"));
    /// Main toolbar → Quick settings → Realtime viewport (ADR-7754).
    QUICK_REALTIME = ("quick.realtime", "Realtime viewport", "quick_settings", Command("view.toggle_realtime"));
    /// Main toolbar → Quick settings → Grid (the viewport's Show menu, ADR-7754).
    QUICK_SHOW_GRID = ("quick.show_grid", "Grid", "quick_settings", Method("editor.set_viewport"));
    /// Main toolbar → Quick settings → Icons (ADR-7754).
    QUICK_SHOW_ICONS = ("quick.show_icons", "Icons", "quick_settings", Method("editor.set_viewport"));
    /// Main toolbar → Quick settings → Bounds (ADR-7754).
    QUICK_SHOW_BOUNDS = ("quick.show_bounds", "Bounds", "quick_settings", Method("editor.set_viewport"));
    /// Main toolbar → Quick settings → Colliders (ADR-7754).
    QUICK_SHOW_COLLIDERS = ("quick.show_colliders", "Colliders", "quick_settings", Method("editor.set_viewport"));
    /// Main toolbar → Quick settings → Stats (ADR-7754).
    QUICK_SHOW_STATS = ("quick.show_stats", "Stats", "quick_settings", Method("editor.set_viewport"));
    /// Main toolbar → Quick settings → Preview sun and sky (ADR-7754).
    QUICK_PREVIEW_SUN = ("quick.preview_sun", "Preview sun and sky", "quick_settings", Method("editor.set_viewport"));
    /// Main toolbar → Quick settings → Autosave (Preferences ▸ Autosave interval, ADR-7754).
    QUICK_AUTOSAVE = ("quick.autosave", "Autosave", "quick_settings", Method("editor.prefs_set"));
    /// Viewport toolbar → Orbit selection (ADR-7755): `editor.set_viewport { orbit_selection }`.
    VIEWPORT_ORBIT_SELECTION = ("viewport.orbit_selection", "Orbit selection", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Camera → Zoom toward cursor (`editor.set_viewport { zoom_to_cursor }`).
    VIEWPORT_ZOOM_TO_CURSOR = ("viewport.zoom_to_cursor", "Zoom toward cursor", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Camera → Speed scales with distance (`editor.set_viewport { speed_with_distance }`).
    VIEWPORT_SPEED_WITH_DISTANCE = ("viewport.speed_with_distance", "Speed scales with distance", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Camera → Invert → Orbit X (`editor.set_viewport { invert }`).
    VIEWPORT_INVERT_ORBIT_X = ("viewport.invert_orbit_x", "Orbit X", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Camera → Invert → Orbit Y.
    VIEWPORT_INVERT_ORBIT_Y = ("viewport.invert_orbit_y", "Orbit Y", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Camera → Invert → Pan X.
    VIEWPORT_INVERT_PAN_X = ("viewport.invert_pan_x", "Pan X", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Camera → Invert → Pan Y.
    VIEWPORT_INVERT_PAN_Y = ("viewport.invert_pan_y", "Pan Y", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Camera → Invert → Fly look Y.
    VIEWPORT_INVERT_FLY_Y = ("viewport.invert_fly_y", "Fly look Y", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Camera → Invert → Wheel.
    VIEWPORT_INVERT_WHEEL = ("viewport.invert_wheel", "Wheel", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Camera → Move camera to selection (`editor.camera_to_selection`).
    VIEWPORT_CAMERA_TO_SELECTION = ("viewport.camera_to_selection", "Move camera to selection", "viewport.toolbar", Command("view.camera_to_selection"));
    /// Viewport toolbar → Camera → Move selection to camera (`editor.selection_to_camera`).
    VIEWPORT_SELECTION_TO_CAMERA = ("viewport.selection_to_camera", "Move selection to camera", "viewport.toolbar", Command("view.selection_to_camera"));
    /// View → Move camera to selection (ADR-7755).
    VIEW_CAMERA_TO_SELECTION = ("view.camera_to_selection", "Move camera to selection", "shell.view", Command("view.camera_to_selection"));
    /// View → Move selection to camera (ADR-7755).
    VIEW_SELECTION_TO_CAMERA = ("view.selection_to_camera", "Move selection to camera", "shell.view", Command("view.selection_to_camera"));
    /// Viewport toolbar → Camera → Pilot selection, and Ctrl+Shift+L (`editor.pilot { entity }`, ADR-7768).
    VIEW_PILOT = ("viewport.pilot", "Pilot selection", "viewport.toolbar", Method("editor.pilot"));
    /// The pilot banner's and the Camera menu's Eject (`editor.pilot { entity: null }`, ADR-7768).
    VIEW_EJECT = ("viewport.eject", "Eject", "viewport.toolbar", Method("editor.pilot"));
    /// Viewport toolbar → Camera → a scene camera: look through it (`editor.look_through { entity }`, ADR-7768).
    VIEW_LOOK_THROUGH = ("viewport.look_through", "Look through", "viewport.toolbar", Method("editor.look_through"));
    /// Viewport toolbar → Camera → Pilot `<camera>` (`editor.pilot { entity }`, ADR-7768).
    VIEW_PILOT_CAMERA = ("viewport.pilot_camera", "Pilot camera", "viewport.toolbar", Method("editor.pilot"));
    /// Viewport toolbar → Camera → Editor camera, and the look-through banner's button (`editor.look_through { entity: null }`).
    VIEW_EDITOR_CAMERA = ("viewport.editor_camera", "Editor camera", "viewport.toolbar", Method("editor.look_through"));
    /// Viewport toolbar → Camera → preview size (`editor.camera_preview { size }`, ADR-7768).
    VIEW_CAMERA_PREVIEW = ("viewport.camera_preview", "Camera preview size", "viewport.toolbar", Method("editor.camera_preview"));
    /// Viewport toolbar → Camera → Rule of thirds (`editor.set_viewport { guides: { thirds } }`, ADR-7768).
    VIEW_GUIDE_THIRDS = ("viewport.guide_thirds", "Rule of thirds", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Camera → Center cross (`editor.set_viewport { guides: { center } }`).
    VIEW_GUIDE_CENTER = ("viewport.guide_center", "Center cross", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Camera → Safe areas (`editor.set_viewport { guides: { safe_areas } }`).
    VIEW_GUIDE_SAFE = ("viewport.guide_safe_areas", "Safe areas", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Camera → a letterbox aspect (`editor.set_viewport { guides: { letterbox } }`).
    VIEW_GUIDE_LETTERBOX = ("viewport.guide_letterbox", "Letterbox", "viewport.toolbar", Method("editor.set_viewport"));
    /// File → Screenshot… (opens the dialog, `editor.command { run: "file.screenshot" }`; the capture
    /// itself is `editor.screenshot`, ADR-7768).
    FILE_SCREENSHOT = ("file.screenshot", "Screenshot…", "shell.file", Command("file.screenshot"));
    /// Screenshot dialog → Size by multiplier (off: an explicit width and height).
    SCREENSHOT_BY_MULTIPLIER = ("screenshot.by_multiplier", "Size by multiplier", "screenshot_dialog", Method("editor.screenshot"));
    /// Screenshot dialog → Editor helpers (`editor.screenshot { helpers }`).
    SCREENSHOT_HELPERS = ("screenshot.helpers", "Editor helpers", "screenshot_dialog", Method("editor.screenshot"));
    /// Screenshot dialog → Take screenshot (`editor.screenshot`).
    SCREENSHOT_TAKE = ("screenshot.take", "Take screenshot", "screenshot_dialog", Method("editor.screenshot"));
    /// The screenshot toast → Open folder (opens the OS file manager; no editor state).
    SCREENSHOT_OPEN_FOLDER = ("screenshot.open_folder", "Open folder", "screenshot_dialog", ActionPath::ViewOnly);
    /// The screenshot toast → Dismiss (`editor.screenshot { close: true }`, ADR-8583).
    SCREENSHOT_DISMISS = ("screenshot.dismiss", "Dismiss", "screenshot_dialog", Method("editor.screenshot"));
    /// Edit → Select → All of the same kind (`editor.select_similar { by: "kind" }`, ADR-7769).
    EDIT_SIMILAR_KIND = ("edit.select_similar_kind", "All of the same kind", "shell.edit", Command("selection.similar_kind"));
    /// Edit → Select → All with the same components.
    EDIT_SIMILAR_COMPONENTS = ("edit.select_similar_components", "All with the same components", "shell.edit", Command("selection.similar_components"));
    /// Edit → Select → All using the same asset.
    EDIT_SIMILAR_ASSET = ("edit.select_similar_asset", "All using the same asset", "shell.edit", Command("selection.similar_asset"));
    /// Edit → Select → All in the same layer.
    EDIT_SIMILAR_LAYER = ("edit.select_similar_layer", "All in the same layer", "shell.edit", Command("selection.similar_layer"));
    /// Outliner row menu → Select similar → All of the same kind (the row's targets seed it).
    OUTLINER_SIMILAR_KIND = ("outliner.select_similar_kind", "All of the same kind", "outliner", Method("editor.select_similar"));
    /// Outliner row menu → Select similar → All with the same components.
    OUTLINER_SIMILAR_COMPONENTS = ("outliner.select_similar_components", "All with the same components", "outliner", Method("editor.select_similar"));
    /// Outliner row menu → Select similar → All using the same asset.
    OUTLINER_SIMILAR_ASSET = ("outliner.select_similar_asset", "All using the same asset", "outliner", Method("editor.select_similar"));
    /// Outliner row menu → Select similar → All in the same layer.
    OUTLINER_SIMILAR_LAYER = ("outliner.select_similar_layer", "All in the same layer", "outliner", Method("editor.select_similar"));
    /// Outliner row menu → Selection root (`editor.selection_root { entity, set }`, ADR-7769).
    OUTLINER_SELECTION_ROOT = ("outliner.selection_root", "Selection root (clicks on children select it)", "outliner", Method("editor.selection_root"));
    /// Viewport → Alt+click list → a row (`editor.pick_stack { x, y, select }`, ADR-7769).
    VIEWPORT_PICK_STACK_ROW = ("viewport.pick_stack_row", "Pick", "viewport.pick_stack", Method("editor.pick_stack"));
    /// View → Rendering Debug (view modes, render overrides, pass timings; ADR-7770).
    VIEW_RENDERING_DEBUG = ("view.rendering_debug", "Rendering Debug", "shell.view", Command("view.toggle_rendering_debug"));
    /// Preview scene → Floor toggle (`editor.preview_scene { floor }`, ADR-7771).
    PREVIEW_SCENE_FLOOR = ("preview_scene.floor", "Floor", "preview_scene", Method("editor.preview_scene"));
    /// Preview scene → Rotate toggle (`editor.preview_scene { rotate }`).
    PREVIEW_SCENE_ROTATE = ("preview_scene.rotate", "Rotate", "preview_scene", Method("editor.preview_scene"));
    /// Mesh viewer → Add simple collision (`editor.mesh_helper { action: "add_collision" }`).
    VIEWER_ADD_COLLISION = ("asset_viewer.add_collision", "Add simple collision", "asset_viewer", Method("editor.mesh_helper"));
    /// Mesh viewer → Generate lightmap UV (`editor.mesh_helper { action: "lightmap_uv" }`).
    VIEWER_LIGHTMAP_UV = ("asset_viewer.lightmap_uv", "Generate lightmap UV", "asset_viewer", Method("editor.mesh_helper"));
    /// Mesh viewer → Build nav preview here (`editor.mesh_helper { action: "nav_preview" }`).
    VIEWER_NAV_PREVIEW = ("asset_viewer.nav_preview", "Build nav preview here", "asset_viewer", Method("editor.mesh_helper"));
    /// Content Browser mesh menu → Add simple collision.
    CONTENT_ADD_COLLISION = ("content.add_collision", "Add simple collision", "content", Method("editor.mesh_helper"));
    /// Content Browser mesh menu → Generate lightmap UV.
    CONTENT_LIGHTMAP_UV = ("content.lightmap_uv", "Generate lightmap UV", "content", Method("editor.mesh_helper"));
    /// Content Browser mesh menu → Build nav preview here.
    CONTENT_NAV_PREVIEW = ("content.nav_preview", "Build nav preview here", "content", Method("editor.mesh_helper"));
    /// Inspector → a field's context menu → Copy value (the field label's Ctrl+C) (`editor.property_copy`).
    INSPECTOR_COPY_VALUE = ("inspector.copy_value", "Copy value", "inspector", Method("editor.property_copy"));
    /// Inspector → a field's context menu → Paste value (the field label's Ctrl+V) (`editor.property_paste`).
    INSPECTOR_PASTE_VALUE = ("inspector.paste_value", "Paste value", "inspector", Method("editor.property_paste"));
    /// Inspector → a field's context menu → Copy as RON (`editor.property_copy` answers `ron`).
    INSPECTOR_COPY_RON = ("inspector.copy_ron", "Copy as RON", "inspector", Method("editor.property_copy"));
    /// Inspector → a field's context menu → Copy agent path (`editor.property_copy` answers `agent_call`).
    INSPECTOR_COPY_AGENT_PATH = ("inspector.copy_agent_path", "Copy agent path", "inspector", Method("editor.property_copy"));
    /// Inspector → a field's reset arrow, or its context menu → Reset to default (`editor.reset_property`).
    INSPECTOR_RESET_FIELD = ("inspector.reset_field", "Reset to default", "inspector", Method("editor.reset_property"));
    /// Inspector → a component's header menu or tools row → Copy component (`editor.component_copy`).
    INSPECTOR_COPY_COMPONENT = ("inspector.copy_component", "Copy component", "inspector", Method("editor.component_copy"));
    /// Inspector → a component's header menu or tools row → Paste component values (`editor.component_paste`).
    INSPECTOR_PASTE_COMPONENT = ("inspector.paste_component", "Paste component values", "inspector", Method("editor.component_paste"));
    /// Inspector → Paste as new component (`editor.component_paste { as_new: true }`).
    INSPECTOR_PASTE_NEW_COMPONENT = ("inspector.paste_new_component", "Paste as new component", "inspector", Method("editor.component_paste"));
    /// Inspector → a component's header menu or tools row → Reset component (`editor.reset_property { all: true }`).
    INSPECTOR_RESET_COMPONENT = ("inspector.reset_component", "Reset component", "inspector", Method("editor.reset_property"));
    /// Inspector header → Raw: hidden fields and structural components, read-only (`editor.inspect_selection { raw }`).
    INSPECTOR_RAW_VIEW = ("inspector.raw_view", "Raw", "inspector", Method("editor.inspect_selection"));
    /// Inspector → an optional field's or @Toggle group's header checkbox (`editor.set_property`).
    INSPECTOR_TOGGLE_GROUP = ("inspector.toggle_group", "Enable", "inspector", Method("editor.set_property"));
    /// Inspector → a list item's Duplicate button (`editor.list_duplicate`).
    INSPECTOR_LIST_DUPLICATE = ("inspector.list_duplicate", "Duplicate item", "inspector", Method("editor.list_duplicate"));
    /// Inspector → a list's Clear (asks to confirm) (`editor.set_property` with `[]`).
    INSPECTOR_LIST_CLEAR = ("inspector.list_clear", "Clear", "inspector", Method("editor.set_property"));
    /// Inspector → a list element's drag grip (drag, or Alt+Up / Alt+Down with focus) (`editor.list_move`).
    INSPECTOR_LIST_MOVE = ("inspector.list_move", "Drag to reorder", "inspector", Method("editor.list_move"));
    /// View → Bulk Edit: the grid of the selection (`editor.bulk_edit_open`; ADR-7787).
    VIEW_BULK_EDIT = ("view.bulk_edit", "Bulk Edit", "shell.view", Method("editor.bulk_edit_open"));
    /// Outliner row menu → Edit in grid: the targets in the Bulk Edit grid (ADR-7787).
    OUTLINER_EDIT_IN_GRID = ("outliner.edit_in_grid", "Edit in grid", "outliner", Method("editor.bulk_edit_open"));
    /// Content Browser → a data asset → Edit in grid: the folder's assets of its type (ADR-7787).
    CONTENT_EDIT_IN_GRID = ("content.edit_in_grid", "Edit in grid", "content_browser", Method("editor.bulk_edit_open"));
    /// View → Asset Jobs (the window of import, cook, thumbnail and package jobs; ADR-7788).
    VIEW_ASSET_JOBS = ("view.asset_jobs", "Asset Jobs", "shell.view", Command("window.asset_jobs"));
    /// Tools → Project Health: the panel's tab, brought forward (ADR-7789).
    TOOLS_PROJECT_HEALTH = ("tools.project_health", "Project Health", "shell.tools", Command("window.open_project_health"));
    /// Prefab overrides window → a row's Apply (into the "Apply to" target, ADR-7790).
    PREFAB_OVERRIDES_APPLY = ("prefab_overrides.apply", "Apply", "prefab_overrides", Method("editor.prefab_overrides_apply"));
    /// Prefab overrides window → a row's Revert.
    PREFAB_OVERRIDES_REVERT = ("prefab_overrides.revert", "Revert", "prefab_overrides", Method("editor.prefab_overrides_revert"));
    /// Prefab overrides window → Apply all (one undo step).
    PREFAB_OVERRIDES_APPLY_ALL = ("prefab_overrides.apply_all", "Apply all", "prefab_overrides", Method("editor.prefab_overrides_apply"));
    /// Prefab overrides window → Revert all (one undo step).
    PREFAB_OVERRIDES_REVERT_ALL = ("prefab_overrides.revert_all", "Revert all", "prefab_overrides", Method("editor.prefab_overrides_revert"));
    /// Prefab overrides window → Close.
    PREFAB_OVERRIDES_CLOSE = ("prefab_overrides.close", "Close", "prefab_overrides", ActionPath::ViewOnly);
    /// Prefab overrides window → Edit in place (the level stays drawn around the prefab).
    PREFAB_EDIT_IN_PLACE = ("prefab_overrides.edit_in_place", "Edit in place", "prefab_overrides", Method("editor.prefab_edit_in_place"));
    /// Inspector → an override row → Overrides… (opens the window).
    INSPECTOR_OPEN_OVERRIDES = ("inspector.open_overrides", "Overrides…", "inspector", Method("editor.prefab_overrides_open"));
    /// Outliner → a prefab instance's override dot (opens the Overrides window).
    OUTLINER_OPEN_OVERRIDES = ("outliner.open_overrides", "Overrides", "outliner", Method("editor.prefab_overrides_open"));
    /// Edit → Prefab overrides… (the Overrides window for the selected instance).
    EDIT_PREFAB_OVERRIDES = ("edit.prefab_overrides", "Prefab overrides…", "shell.edit", Command("prefab.overrides"));
    /// Edit → Edit prefab in place (the selected instance's prefab, the level drawn around it).
    EDIT_PREFAB_IN_PLACE = ("edit.prefab_in_place", "Edit prefab in place", "shell.edit", Command("prefab.edit_in_place"));
    /// Layers panel → a layer row → Lock (members cannot be picked in the viewport, ADR-7792).
    LAYERS_LOCK = ("layers.lock", "Lock", "layers_panel", Method("editor.layer_lock"));
    /// Layers panel → a layer row → Store in its own file (`<level>.<layer>.layer.ron`, ADR-7792).
    LAYERS_OWN_FILE = ("layers.own_file", "Store in its own file", "layers_panel", Method("editor.layer_file"));
    /// Layers panel → a layer row stored in its own file → Merge back into the level.
    LAYERS_MERGE_FILE = ("layers.merge_file", "Merge back into the level", "layers_panel", Method("editor.layer_file"));
    /// Layers panel → Open levels → Open beside (a level opened next to the host, ADR-7792).
    LEVELS_OPEN = ("levels.open", "Open beside", "layers_panel", Method("editor.levels_open"));
    /// Layers panel → Open levels → a document's Save (writes only its file).
    LEVELS_SAVE = ("levels.save", "Save", "layers_panel", Method("editor.levels_open"));
    /// Layers panel → Open levels → a level's Close (asks only about its own changes).
    LEVELS_CLOSE = ("levels.close", "Close", "layers_panel", Method("editor.levels_open"));
    /// Layers panel → Open levels → Active (new entities land in this level).
    LEVELS_ACTIVE = ("levels.active", "Active", "layers_panel", Method("editor.active_level"));
    /// Content Browser → a RON asset's menu → Diff against HEAD… (`editor.asset_diff`, ADR-7799).
    CONTENT_DIFF_HEAD = ("content.diff_head", "Diff against HEAD…", "content_browser", Method("editor.asset_diff"));
    /// Content Browser → a RON asset's menu → Diff against revision… (the picker lists `editor.asset_revisions`).
    CONTENT_DIFF_REVISION = ("content.diff_revision", "Diff against revision…", "content_browser", Method("editor.asset_revisions"));
    /// Material and animation graph panels → file row → Diff against HEAD… (the open graph's saved file).
    GRAPH_DIFF_HEAD = ("graph.diff_head", "Diff against HEAD…", "graph_panels", Method("editor.asset_diff"));
    /// Material and animation graph panels → file row → Diff against revision….
    GRAPH_DIFF_REVISION = ("graph.diff_revision", "Diff against revision…", "graph_panels", Method("editor.asset_revisions"));
    /// Diff window → Pick another revision… (back to the history list).
    DIFF_PICK_ANOTHER = ("asset_diff.pick_another", "Pick another revision…", "asset_diff", Method("editor.asset_revisions"));
    /// Diff window → Zoom in (both graph canvases).
    DIFF_ZOOM_IN = ("asset_diff.zoom_in", "Zoom in", "asset_diff", ActionPath::ViewOnly);
    /// Diff window → Zoom out (both graph canvases).
    DIFF_ZOOM_OUT = ("asset_diff.zoom_out", "Zoom out", "asset_diff", ActionPath::ViewOnly);
    /// Diff window → Fit (both graph canvases).
    DIFF_FIT = ("asset_diff.fit", "Fit", "asset_diff", ActionPath::ViewOnly);
    /// Diff window → Show unchanged fields (the tree view).
    DIFF_SHOW_UNCHANGED = ("asset_diff.show_unchanged", "Show unchanged fields", "asset_diff", ActionPath::ViewOnly);
    /// World Map → Open world (the path field's streamed level; ADR-7802).
    WORLD_OPEN = ("world_map.open", "Open world", "world_map", Method("editor.world_open"));
    /// World Map → Load selected (a cancellable `world_load:<n>` job).
    WORLD_LOAD_SELECTED = ("world_map.load_selected", "Load selected", "world_map", Method("editor.world_load"));
    /// World Map → Unload selected (asks first when a cell has unsaved edits).
    WORLD_UNLOAD_SELECTED = ("world_map.unload_selected", "Unload selected", "world_map", Method("editor.world_unload"));
    /// World Map → Clear selection (panel view state).
    WORLD_SELECT_NONE = ("world_map.select_none", "Clear selection", "world_map", ActionPath::ViewOnly);
    /// World Map → Load around camera (`editor.world_load { radius_m }`).
    WORLD_LOAD_AROUND = ("world_map.load_around_camera", "Load around camera", "world_map", Method("editor.world_load"));
    /// World Map → Cancel load, while a load runs (`editor.cancel_job { id: "world_load:<n>" }`).
    WORLD_LOAD_CANCEL = ("world_map.load_cancel", "Cancel load", "world_map", Method("editor.cancel_job"));
    /// World Map → unsaved edits prompt → Save and unload (`editor.world_unload { save: true }`).
    WORLD_UNLOAD_SAVE = ("world_map.unload_save", "Save and unload", "world_map", Confirm("editor.world_unload"));
    /// World Map → unsaved edits prompt → Discard and unload (`editor.world_unload { save: false }`).
    WORLD_UNLOAD_DISCARD = ("world_map.unload_discard", "Discard and unload", "world_map", Confirm("editor.world_unload"));
    /// World Map → unsaved edits prompt → Keep loaded (closes the prompt).
    WORLD_UNLOAD_KEEP = ("world_map.unload_keep", "Keep loaded", "world_map", ActionPath::ViewOnly);
    /// Remaining AI navigation paths, including in play.
    VIEW_SHOW_PATHS = ("viewport.show_paths", "Paths", "viewport.toolbar", Method("editor.set_viewport"));
    /// Terrain rectangle selection changes only the session's view state.
    TERRAIN_REGION_MODE = ("terrain.region_mode", "Region mode", "terrain_panel", ActionPath::ViewOnly);
    /// Copy the selected height and weight rectangle.
    TERRAIN_REGION_COPY = ("terrain.region_copy", "Copy region", "terrain_panel", Method("editor.terrain_region"));
    /// Paste the clipboard as one undo step.
    TERRAIN_REGION_PASTE = ("terrain.region_paste", "Paste region", "terrain_panel", Method("editor.terrain_region"));
    /// Export the selection through heightmap interchange.
    TERRAIN_REGION_EXPORT = ("terrain.region_export", "Export region", "terrain_panel", Method("editor.terrain_region"));
    /// Import a heightmap into the selection.
    TERRAIN_REGION_IMPORT = ("terrain.region_import", "Import region", "terrain_panel", Method("editor.terrain_region"));
    /// Viewport kit grid mode.
    KIT_PAINT_MODE = ("kit.paint_mode", "Kit painting", "viewport.toolbar", Method("editor.kit_paint"));
    /// Refresh the kit palette from the selected folder.
    KIT_PAINT_REFRESH = ("kit.paint_refresh", "Refresh pieces", "kit_paint", Method("editor.kit_paint"));
    /// Select one piece in the kit palette.
    KIT_PAINT_PIECE = ("kit.paint_piece", "Choose piece", "kit_paint", Method("editor.kit_paint"));
    /// Place at the keyboard cursor.
    KIT_PAINT_PLACE = ("kit.paint_place", "Place at cell", "kit_paint", Method("editor.kit_paint"));
    /// Erase at the keyboard cursor.
    KIT_PAINT_ERASE = ("kit.paint_erase", "Erase at cell", "kit_paint", Method("editor.kit_paint"));
    /// Byte order for region RAW interchange.
    TERRAIN_REGION_ENDIAN = ("terrain.region_endian", "Region RAW big-endian", "terrain_panel", Method("editor.terrain_region"));
    /// Create > Blockout brushes (ADR-7809).
    CREATE_BLOCKOUT = ("create.blockout", "Blockout brush", "blockout", Method("editor.blockout_add"));
    /// Bake the selected brush group (ADR-7809).
    BAKE_BLOCKOUT = ("blockout.bake", "Bake to mesh", "blockout", Method("editor.blockout_bake"));
    /// Theme panel: create a variation with a base type.
    THEME_VARIATION = ("theme.variation", "New variation", "theme", Method("editor.theme_panel_set"));
    /// Theme panel: set a typed property.
    THEME_PROPERTY = ("theme.property", "Set property", "theme", Method("editor.theme_panel_set"));
    /// Theme panel: remove a property override.
    THEME_RESET = ("theme.reset", "Reset override", "theme", Method("editor.theme_panel_set"));
    /// Theme panel: pin a style box for copying.
    THEME_PIN = ("theme.pin", "Pin style", "theme", ActionPath::ViewOnly);
    /// Theme panel: copy a pinned style to every state.
    THEME_ALL_STATES = ("theme.all_states", "Apply to all states", "theme", Method("editor.theme_panel_set"));
    /// Theme panel: import selected items.
    THEME_IMPORT = ("theme.import", "Import items", "theme", Method("editor.theme_panel_import"));
    /// Theme panel: undo the last draft edit.
    THEME_UNDO = ("theme.undo", "Undo theme edit", "theme", Method("editor.theme_panel_set"));
    /// Theme panel: redo the last undone draft edit.
    THEME_REDO = ("theme.redo", "Redo theme edit", "theme", Method("editor.theme_panel_set"));

    /// Theme gallery: add a user UI document preview page.
    THEME_PREVIEW_DOCUMENT = ("theme.preview_document", "Add preview document", "theme", Method("editor.theme_panel_set"));

    /// Theme panel: author a widget type or part.
    THEME_TYPE = ("theme.type", "Add type", "theme", Method("editor.theme_panel_set"));

    /// Content Browser selection menu → Validate the selection (ADR-7822).
    CONTENT_BULK_VALIDATE = ("content.bulk_validate", "Validate selection", "content_browser", Method("editor.bulk_asset_action"));
    /// Content Browser selection menu → Re-import the selection.
    CONTENT_BULK_REIMPORT = ("content.bulk_reimport", "Re-import selection", "content_browser", Method("editor.bulk_asset_action"));
    /// Content Browser selection menu → Save the selection's unsaved documents.
    CONTENT_BULK_SAVE = ("content.bulk_save", "Save selection", "content_browser", Method("editor.bulk_asset_action"));
    /// Content Browser selection menu → Move to… (the destination folder row).
    CONTENT_BULK_MOVE = ("content.bulk_move", "Move to…", "content_browser", Method("editor.bulk_asset_action"));
    /// Content Browser selection menu → Delete… (one confirmation listing every referencer).
    CONTENT_BULK_DELETE = ("content.bulk_delete", "Delete selection…", "content_browser", Method("editor.bulk_asset_action"));
    /// Content Browser selection menu → Add to collection (one item per static collection).
    CONTENT_BULK_COLLECTION = ("content.bulk_add_to_collection", "Add to collection", "content_browser", Method("editor.bulk_asset_action"));
    /// Content Browser bulk delete confirmation → Delete all.
    CONTENT_BULK_DELETE_CONFIRM = ("content.bulk_delete_confirm", "Delete all", "content_browser", Confirm("editor.bulk_asset_action"));
    /// Content Browser bulk move row → Move.
    CONTENT_BULK_MOVE_CONFIRM = ("content.bulk_move_confirm", "Move here", "content_browser", Method("editor.bulk_asset_action"));
    /// Content Browser confirmations → Cancel (bulk delete, bulk move, delete).
    CONTENT_ACTION_CANCEL = ("content.action_cancel", "Cancel", "content_browser", ActionPath::ViewOnly);
    /// Content Browser delete confirmation → Replace references with… (`editor.delete_asset { replace_with }`).
    CONTENT_DELETE_REPLACE = ("content.delete_replace", "Replace references with…", "content_browser", Method("editor.delete_asset"));
    /// Reference graph / Problems missing reference → Retarget… (the asset picker).
    MISSING_RETARGET = ("asset_panels.retarget", "Retarget…", "asset_panels", Method("editor.retarget_references"));
    /// Reference graph / Problems missing reference → Find similar (`similar: true`).
    MISSING_FIND_SIMILAR = ("asset_panels.find_similar", "Find similar", "asset_panels", Method("editor.retarget_references"));
    /// Asset picker → Apply (retarget, or replace and delete).
    RETARGET_APPLY = ("retarget.apply", "Apply", "retarget_picker", Method("editor.retarget_references"));
    /// Asset picker → Cancel.
    RETARGET_CANCEL = ("retarget.cancel", "Cancel", "retarget_picker", ActionPath::ViewOnly);
    /// Save all dialog → Save selected (`editor.save_all { ids }`).
    SAVE_ALL_SAVE_SELECTED = ("save_all.save_selected", "Save selected", "save_all", Method("editor.save_all"));
    /// Save all dialog → Don't save (on close the agent calls `agent.shutdown`).
    SAVE_ALL_DONT_SAVE = ("save_all.dont_save", "Don't save", "save_all", Confirm("editor.save_all"));
    /// Save all dialog → Cancel, or Close with nothing unsaved (`editor.save_all { cancel: true }`, ADR-8583).
    SAVE_ALL_CANCEL = ("save_all.cancel", "Cancel", "save_all", Method("editor.save_all"));
    /// Outliner/Inspector source switch → Live link (`editor.live_link { on }`, ADR-7833).
    LIVE_LINK = ("remote.live_link", "Live link", "remote_world", Method("editor.live_link"));
    /// Remote Outliner → More (the next page of the runtime's entities; `editor.remote_select { pages }`, ADR-7833).
    REMOTE_MORE = ("remote.more", "More", "remote_world", Method("editor.remote_select"));
    /// Undo History → jump to a retained history boundary.
    HISTORY_JUMP = ("history.jump", "Jump to history entry", "undo_history", Method("editor.history_jump"));

    /// Window → Undo History.
    WINDOW_UNDO_HISTORY = ("window.undo_history", "Undo History", "shell.window", Command("window.open_undo_history"));

    /// Recovery banner → normal restart.
    SAFE_MODE_RESTART = ("safe_mode.restart", "Restart normally", "safe_mode", Method("editor.safe_mode_restart"));
    /// Startup recovery → dismiss the explanation and keep safe mode.
    SAFE_MODE_CONTINUE = ("safe_mode.continue", "Continue in safe mode", "safe_mode", ActionPath::ViewOnly);

    /// View → Revision Control (the git window, ADR-7837).
    VIEW_REVISION_CONTROL = ("view.revision_control", "Revision Control", "shell.view", Command("view.toggle_revision_control"));
    /// View → Color Vision → (one per filter): `editor.color_vision { mode, deficiency }` (ADR-7838).
    VIEW_COLOR_VISION = ("view.color_vision", "Color Vision", "shell.view", Method("editor.color_vision"));
    /// Window → Platforms.
    PLATFORMS_OPEN = ("platforms.open", "Platforms", "shell.window", Method("editor.platform_check"));
    /// Re-run platform diagnostics.
    PLATFORMS_RECHECK = ("platforms.recheck", "Re-check", "platforms", Method("editor.platform_check"));
    /// Window → Launch Profiles, also on the launch bar.
    LAUNCH_PROFILES_OPEN = ("launch_profiles.open", "Launch Profiles", "shell.window", Method("editor.launch_profiles"));
    /// Save the launch profile form.
    LAUNCH_PROFILE_SAVE = ("launch_profiles.save", "Save profile", "launch_profiles", Method("editor.launch_profiles"));
    /// Delete the selected saved launch profile.
    LAUNCH_PROFILE_DELETE = ("launch_profiles.delete", "Delete profile", "launch_profiles", Method("editor.launch_profiles"));
    /// Start a new unsaved profile form.
    LAUNCH_PROFILE_NEW = ("launch_profiles.new", "New profile", "launch_profiles", ActionPath::ViewOnly);
    /// Include the package step in a saved profile.
    LAUNCH_PROFILE_PACKAGE = ("launch_profiles.package", "Package before launch", "launch_profiles", Method("editor.launch_profiles"));
    /// Add an unsaved application argument field.
    LAUNCH_PROFILE_ADD_ARG = ("launch_profiles.add_arg", "Add argument", "launch_profiles", Method("editor.launch_profiles"));
    /// Remove an unsaved application argument field.
    LAUNCH_PROFILE_REMOVE_ARG = ("launch_profiles.remove_arg", "Remove argument", "launch_profiles", Method("editor.launch_profiles"));

    /// Window → Size Report.
    SIZE_REPORT_OPEN = ("size_report.open", "Size Report", "shell.window", Method("editor.package_size_report"));
    /// Refresh measured package contents.
    SIZE_REPORT_REFRESH = ("size_report.refresh", "Refresh report", "size_report", Method("editor.package_size_report"));

    /// Window → Notes.
    WINDOW_NOTES = ("window.notes", "Notes", "shell.window", Command("window.open_notes"));
    /// Window → Activity.
    WINDOW_ACTIVITY = ("window.activity", "Activity", "shell.window", Command("window.open_activity"));
    /// Notes → Add note.
    NOTE_ADD = ("notes.add", "Add note", "notes", Method("editor.note_add"));
    /// Notes → Reply.
    NOTE_REPLY = ("notes.reply", "Reply", "notes", Method("editor.note_reply"));
    /// Notes → Resolve.
    NOTE_RESOLVE = ("notes.resolve", "Resolve", "notes", Method("editor.note_resolve"));
    /// Notes → Reopen.
    NOTE_REOPEN = ("notes.reopen", "Reopen", "notes", Method("editor.note_resolve"));
    /// Notes and pins → Frame marker (selection has editor.select).
    NOTE_FRAME = ("notes.frame", "Frame note", "notes", Method("editor.focus"));
    /// Activity → Select entity.
    ACTIVITY_SELECT = ("activity.select", "Select entity", "activity", Method("editor.select"));
    /// File → New level variant… (pick a base level, then the new `.levelvariant.ron`; ADR-7823).
    FILE_NEW_LEVEL_VARIANT = ("file.new_level_variant", "New level variant…", "shell.file", OsDialog("editor.new_level_variant"));
    /// Inspector → an entity of an open level variant → Revert entity to base.
    INSPECTOR_REVERT_LEVEL_VARIANT_ENTITY = ("inspector.revert_level_variant_entity", "Revert entity to base", "inspector", Method("editor.level_variant_revert"));
    /// Inspector → a field an open level variant overrides → Revert to base.
    INSPECTOR_REVERT_LEVEL_VARIANT_FIELD = ("inspector.revert_level_variant_field", "Revert to base", "inspector", Method("editor.level_variant_revert"));
    /// Terrain panel → Edit layers → Add layer (ADR-7825).
    TERRAIN_LAYER_ADD = ("terrain.edit_layer_add", "Add layer", "terrain_panel", Method("terrain.edit_layer_create"));
    /// Terrain panel → Edit layers → Set height layer (the next layer's blend).
    TERRAIN_LAYER_SET_HEIGHT = ("terrain.edit_layer_set_height", "Set height layer", "terrain_panel", Method("terrain.edit_layer_create"));
    /// Terrain panel → Edit layers → Base (strokes write the base heights).
    TERRAIN_LAYER_BASE = ("terrain.edit_layer_base", "Base", "terrain_panel", Method("terrain.edit_layer_set"));
    /// Terrain panel → Edit layers → a layer row (strokes write it).
    TERRAIN_LAYER_SELECT = ("terrain.edit_layer_select", "Select layer", "terrain_panel", Method("terrain.edit_layer_set"));
    /// Terrain panel → Edit layers → Hide / Show.
    TERRAIN_LAYER_VISIBLE = ("terrain.edit_layer_visible", "Hide", "terrain_panel", Method("terrain.edit_layer_set"));
    /// Terrain panel → Edit layers → Lock / Unlock.
    TERRAIN_LAYER_LOCK = ("terrain.edit_layer_lock", "Lock", "terrain_panel", Method("terrain.edit_layer_set"));
    /// Terrain panel → Edit layers → Move up.
    TERRAIN_LAYER_UP = ("terrain.edit_layer_up", "Move up", "terrain_panel", Method("terrain.edit_layer_reorder"));
    /// Terrain panel → Edit layers → Move down.
    TERRAIN_LAYER_DOWN = ("terrain.edit_layer_down", "Move down", "terrain_panel", Method("terrain.edit_layer_reorder"));
    /// Terrain panel → Edit layers → Delete.
    TERRAIN_LAYER_DELETE = ("terrain.edit_layer_delete", "Delete", "terrain_panel", Method("terrain.edit_layer_delete"));
    /// Terrain panel → Edit layers → Rename (the active layer).
    TERRAIN_LAYER_RENAME = ("terrain.edit_layer_rename", "Rename", "terrain_panel", Method("terrain.edit_layer_set"));
    /// Terrain panel → Edit layers → Flatten into base.
    TERRAIN_LAYER_FLATTEN = ("terrain.edit_layer_flatten", "Flatten into base", "terrain_panel", Method("terrain.edit_layer_flatten"));
    /// Terrain panel → Edit layers → Detach the procgen target (ADR-8381).
    TERRAIN_LAYER_DETACH = ("terrain.edit_layer_detach", "Detach procgen", "terrain_panel", Method("terrain.edit_layer_procgen"));
    /// Terrain panel → Edit layers → Attach a procgen graph and output (ADR-8381).
    TERRAIN_LAYER_ATTACH = ("terrain.edit_layer_attach", "Attach procgen", "terrain_panel", Method("terrain.edit_layer_procgen"));
    /// Terrain panel → Edit layers → Paint a material weight at sample (0, 0) (ADR-8381).
    TERRAIN_LAYER_PAINT_WEIGHT = ("terrain.edit_layer_paint_weight", "Paint weight", "terrain_panel", Method("terrain.edit_layer_paint_weights"));
    /// Play toolbar → Start → From player start (`editor.play { start: "player_start" }`; ADR-7829).
    PLAY_START_PLAYER_START = ("play.start_player_start", "From player start", "shell.play", Method("editor.play"));
    /// Play toolbar → Start → From editor camera (`editor.play { start: "camera" }`; ADR-7829).
    PLAY_START_CAMERA = ("play.start_camera", "From editor camera", "shell.play", Method("editor.play"));
    /// Play toolbar → Start → one of the level's player starts (`editor.play { start: "entity", entity }`; ADR-7829).
    PLAY_START_ENTITY = ("play.start_entity", "From player start…", "shell.play", Method("editor.play"));
    /// Play toolbar → Mute the play world's audio (`editor.play_controls { mute }`; ADR-7829).
    PLAY_MUTE = ("play.mute", "Mute", "shell.play", Method("editor.play_controls"));
    /// Play toolbar → Viewing: Server (`editor.pie_view { role: "server" }`; ADR-7829).
    PIE_VIEW_SERVER = ("play.view_server", "Server", "shell.play", Method("editor.pie_view"));
    /// Play toolbar → Viewing: Client N (`editor.pie_view { role: "client", client }`; ADR-7829).
    PIE_VIEW_CLIENT = ("play.view_client", "Client", "shell.play", Method("editor.pie_view"));
    /// Play toolbar → Device → a device preset or none (`editor.play { window: true, device: { preset } }`; ADR-7829).
    PLAY_DEVICE = ("play.device", "Device", "shell.play", Method("editor.play"));
    /// Play toolbar → Device → Turn (`editor.play { device: { landscape } }`; ADR-7829).
    PLAY_DEVICE_ROTATE = ("play.device_rotate", "Turn device", "shell.play", Method("editor.play"));
    /// Play toolbar → Device → Touch emulation (`editor.play { device: { touch } }`; ADR-7829).
    PLAY_DEVICE_TOUCH = ("play.device_touch", "Touch emulation", "shell.play", Method("editor.play"));
    /// Play toolbar → Device → Virtual gamepad (`editor.play { device: { gamepad } }`; ADR-7829).
    PLAY_DEVICE_GAMEPAD = ("play.device_gamepad", "Virtual gamepad", "shell.play", Method("editor.play"));
    /// Play toolbar → Device → Safe-area outline (`editor.play { device: { safe_area_outline } }`).
    PLAY_DEVICE_SAFE_AREA = ("play.device_safe_area", "Safe-area outline", "shell.play", Method("editor.play"));
    /// Collaboration → Host.
    COLLAB_HOST = ("collab.host", "Host", "collaboration", Method("editor.collab_host"));
    /// Collaboration → Join.
    COLLAB_JOIN = ("collab.join", "Join", "collaboration", Method("editor.collab_join"));
    /// Collaboration → Leave.
    COLLAB_LEAVE = ("collab.leave", "Leave", "collaboration", Method("editor.collab_leave"));
    /// Collaboration → Accept a pending participant.
    COLLAB_ACCEPT = ("collab.accept", "Accept", "collaboration", Method("editor.collab_host"));
    /// Collaboration → Reject a pending participant.
    COLLAB_REJECT = ("collab.reject", "Reject", "collaboration", Method("editor.collab_host"));
    /// Collaboration → Jump to a remote participant.
    COLLAB_JUMP = ("collab.jump", "Jump to", "collaboration", Method("editor.collab_jump"));

    /// Hosted Agent panel: Start.
    AGENT_START = ("agent.start", "Start", "agent", Method("editor.agent_sessions"));
    /// Hosted Agent panel: Stop.
    AGENT_STOP = ("agent.stop", "Stop", "agent", Method("editor.agent_sessions"));
    /// Hosted Agent panel: Send.
    AGENT_SEND = ("agent.send", "Send", "agent", Method("editor.agent_sessions"));
    /// Hosted Agent panel: Session mode.
    AGENT_MODE = ("agent.mode", "Session mode", "agent", Method("editor.agent_sessions"));
    /// Hosted Agent panel: Approve plan.
    AGENT_APPROVE = ("agent.approve", "Approve plan", "agent", Confirm("editor.agent_sessions"));
    /// Hosted Agent panel: Reject plan.
    AGENT_REJECT = ("agent.reject", "Reject plan", "agent", Confirm("editor.agent_sessions"));
    /// Hosted Agent panel: Reviewed step.
    AGENT_PLAN_CHECK = ("agent.plan_check", "Reviewed step", "agent", ActionPath::ViewOnly);
    /// Hosted Agent panel: Choose agent.
    AGENT_PICK = ("agent.pick", "Choose agent", "agent", ActionPath::ViewOnly);
    /// Hosted Agent panel: Open Preferences.
    AGENT_PREFERENCES = ("agent.preferences", "Open Preferences", "agent", Command("window.preferences"));
    /// Window → Agent.
    WINDOW_AGENT = ("window.agent", "Agent", "shell.window", Command("window.open_agent"));
    /// Import settings panel → Apply preset (the preset's settings replace the form; ADR-7904).
    IMPORT_PRESET_APPLY = ("import_settings.preset_apply", "Apply preset", "import_settings", Method("editor.import_presets"));
    /// Import settings panel → Delete preset.
    IMPORT_PRESET_DELETE = ("import_settings.preset_delete", "Delete preset", "import_settings", Method("editor.import_presets"));
    /// Import settings panel → Save as preset (the form as a named project preset).
    IMPORT_PRESET_SAVE = ("import_settings.preset_save", "Save as preset", "import_settings", Method("editor.import_presets"));
    /// Import dialog → Import (write each file's .meta, copy and cook once; ADR-7904).
    IMPORT_DIALOG_OK = ("import_dialog.ok", "Import", "import_dialog", Method("editor.import_with"));
    /// Import dialog → Apply to all (the shown settings for every remaining file of the importer, then Import).
    IMPORT_DIALOG_APPLY_ALL = ("import_dialog.apply_to_all", "Apply to all", "import_dialog", Method("editor.import_with"));
    /// Import dialog → Skip this file (leave it out of the import).
    IMPORT_DIALOG_SKIP = ("import_dialog.skip", "Skip file", "import_dialog", Method("editor.import_with"));
    /// Import dialog → Cancel (import nothing).
    IMPORT_DIALOG_CANCEL = ("import_dialog.cancel", "Cancel", "import_dialog", Method("editor.import_dialog_open"));
    /// Import dialog → Apply preset to the shown file.
    IMPORT_DIALOG_PRESET = ("import_dialog.preset_apply", "Apply preset", "import_dialog", Method("editor.import_with"));
    /// Import dialog → a clip's Import toggle (import or skip the animation).
    IMPORT_DIALOG_CLIP_IMPORT = ("import_dialog.clip_import", "Import clip", "import_dialog", Method("editor.import_with"));
    /// Import dialog → a clip's Add range (split the take into another clip).
    IMPORT_DIALOG_CLIP_ADD_RANGE = ("import_dialog.clip_add_range", "Add range", "import_dialog", Method("editor.import_with"));
    /// Import dialog → a range's Remove button.
    IMPORT_DIALOG_CLIP_REMOVE_RANGE = ("import_dialog.clip_remove_range", "Remove range", "import_dialog", Method("editor.import_with"));
    /// Content browser → Re-import with options… (the import dialog over an imported glTF/FBX/OBJ).
    CONTENT_REIMPORT_WITH_OPTIONS = ("content.reimport_with_options", "Re-import with options…", "content_browser", Method("editor.import_with"));
    /// Add the selected asset and dependencies to the user library.
    LIBRARY_ADD = ("library.add", "Add to library", "shell.content_library", Method("editor.asset_library"));
    /// Refresh the user's library entries.
    LIBRARY_LIST = ("library.list", "Refresh library", "shell.content_library", Method("editor.asset_library"));
    /// Copy a library entry into the project with remapped references.
    LIBRARY_USE = ("library.use", "Use in project", "shell.content_library", Method("editor.asset_library"));
    /// Content Browser selection menu and bar → Edit selection in grid: the selected data assets as Bulk Edit rows (P-145, ADR-8147).
    CONTENT_BULK_EDIT_IN_GRID = ("content.bulk_edit_in_grid", "Edit selection in grid", "content_browser", Method("editor.bulk_edit"));
    /// Content Browser selection menu and bar → Export selection with dependencies…: one package of the selection (P-145, ADR-8147).
    CONTENT_BULK_EXPORT = ("content.bulk_export", "Export selection with dependencies…", "content_browser", Method("editor.export_package"));

    /// Profiler ▸ Memory: Categories and snapshots / GPU resources (view-only switch; ADR-8142).
    PROFILER_MEMORY_VIEW = ("profiler.memory_view", "Memory view", "profiler", ActionPath::ViewOnly);
    /// Profiler ▸ Memory ▸ Take snapshot (`editor.memory_snapshot { action: "take", name }`).
    PROFILER_MEMORY_TAKE = ("profiler.memory_take", "Take snapshot", "profiler", Method("editor.memory_snapshot"));
    /// Profiler ▸ Memory ▸ Diff from (the focused snapshot; `editor.memory_snapshot { action: "diff", a, b }`).
    PROFILER_MEMORY_SET_A = ("profiler.memory_set_a", "Diff from", "profiler", Method("editor.memory_snapshot"));
    /// Profiler ▸ Memory ▸ Diff to (the focused snapshot; `editor.memory_snapshot { action: "diff", a, b }`).
    PROFILER_MEMORY_SET_B = ("profiler.memory_set_b", "Diff to", "profiler", Method("editor.memory_snapshot"));
    /// Profiler ▸ Memory ▸ Export JSON (`editor.memory_snapshot { action: "export", name }`).
    PROFILER_MEMORY_EXPORT = ("profiler.memory_export", "Export JSON", "profiler", Method("editor.memory_snapshot"));
    /// Profiler ▸ Memory ▸ GPU resources ▸ Previous mip (`editor.gpu_resources { action: "preview", mip }`).
    PROFILER_GPU_MIP_DOWN = ("profiler.gpu_mip_down", "Previous mip", "profiler", Method("editor.gpu_resources"));
    /// Profiler ▸ Memory ▸ GPU resources ▸ Next mip (`editor.gpu_resources { action: "preview", mip }`).
    PROFILER_GPU_MIP_UP = ("profiler.gpu_mip_up", "Next mip", "profiler", Method("editor.gpu_resources"));
    /// Profiler ▸ Memory ▸ GPU resources ▸ channel RGBA / R / G / B / A (`editor.gpu_resources { action: "preview", channel }`).
    PROFILER_GPU_CHANNEL = ("profiler.gpu_channel", "Channel", "profiler", Method("editor.gpu_resources"));
    /// Profiler ▸ Memory ▸ GPU resources ▸ Live preview (re-reads every 30 frames; an agent calls preview again).
    PROFILER_GPU_LIVE = ("profiler.gpu_live", "Live preview", "profiler", Method("editor.gpu_resources"));
    /// Profiler ▸ Memory ▸ GPU resources ▸ Refresh preview (`editor.gpu_resources { action: "preview" }`).
    PROFILER_GPU_REFRESH = ("profiler.gpu_refresh", "Refresh preview", "profiler", Method("editor.gpu_resources"));
    /// Window → New Outliner (P-003, ADR-8105 (a)).
    WINDOW_NEW_OUTLINER = ("window.new_outliner", "New Outliner", "shell.window", Method("editor.panel_new_instance"));
    /// Window → New Content Browser (P-003, ADR-8105 (a)).
    WINDOW_NEW_CONTENT = ("window.new_content", "New Content Browser", "shell.window", Method("editor.panel_new_instance"));
    /// Window → New Inspector (P-003, ADR-8105 (a)).
    WINDOW_NEW_INSPECTOR = ("window.new_inspector", "New Inspector", "shell.window", Method("editor.panel_new_instance"));
    /// Help → Samples… (opens the samples browser; `editor.samples { action: "list" }` lists what it shows).
    HELP_SAMPLES = ("help.samples", "Samples…", "shell.help", Method("editor.samples"));
    /// Samples window → Open (copies the selected sample into the chosen folder and opens the copy; `editor.samples { action: "create" }`).
    SAMPLES_OPEN = ("samples.open", "Open", "samples", Method("editor.samples"));
    /// Extensions panel → a bundled sample's Open (the same copy; `editor.samples { action: "create" }`).
    EXTENSIONS_SAMPLE_OPEN = ("extensions.sample_open", "Open sample", "extensions", Method("editor.samples"));
    /// Window → Utility Panels → a panel (ADR-8189).
    WINDOW_UTILITY_PANEL = ("window.utility_panel", "Utility panel", "shell.window", Method("editor.utility_panels"));
    /// Workspace area menu: join a neighboring area.
    WORKSPACE_JOIN = ("workspace.join", "Join with neighbor", "shell.workspace", Method("editor.layout_join"));
    /// Workspace area menu: exchange two tab stacks.
    WORKSPACE_SWAP = ("workspace.swap", "Swap with", "shell.workspace", Method("editor.layout_swap"));
    /// Active workspace mode's tool panel buttons.
    WORKSPACE_TOOL = ("workspace.tool", "Open mode tool", "shell.workspace", Method("editor.layout_apply"));
    /// Hosted Agent panel: Approve every staged editor call (P-369, ADR-8375).
    AGENT_STAGED_APPROVE_ALL = ("agent.staged_approve_all", "Approve all staged calls", "agent", Confirm("editor.agent_sessions"));
    /// Hosted Agent panel: Approve the checked staged calls.
    AGENT_STAGED_APPROVE = ("agent.staged_approve", "Approve checked calls", "agent", Confirm("editor.agent_sessions"));
    /// Hosted Agent panel: Reject the checked staged calls.
    AGENT_STAGED_REJECT = ("agent.staged_reject", "Reject checked calls", "agent", Confirm("editor.agent_sessions"));
    /// Hosted Agent panel: Reject every staged editor call.
    AGENT_STAGED_REJECT_ALL = ("agent.staged_reject_all", "Reject all staged calls", "agent", Confirm("editor.agent_sessions"));
    /// Hosted Agent panel: Check a staged call for Approve or Reject checked.
    AGENT_STAGED_CHECK = ("agent.staged_check", "Include staged call", "agent", ActionPath::ViewOnly);
    /// Remote Outliner → Show or hide children of the selected row (`editor.remote_select { collapse, expand }`).
    REMOTE_TOGGLE_CHILDREN = ("remote.toggle_children", "Show or hide children", "remote_world", Method("editor.remote_select"));
    /// File → Version history… (ADR-7928): the window lists `editor.versions`.
    FILE_VERSION_HISTORY = ("file.version_history", "Version history…", "shell.file", Method("editor.versions"));
    /// File → Duplicate project… (ADR-7928): a folder dialog; the agent passes `dest`.
    FILE_DUPLICATE_PROJECT = ("file.duplicate_project", "Duplicate project…", "shell.file", OsDialog("editor.duplicate_project"));
    /// Version History → Preview (read-only).
    VERSIONS_PREVIEW = ("versions.preview", "Preview", "versions", Method("editor.version_preview"));
    /// Version History → Restore (a new undoable change).
    VERSIONS_RESTORE = ("versions.restore", "Restore", "versions", Method("editor.version_restore"));
    /// Viewport toolbar → Show → Fog of war: watchers, grid outline, texture and viewer team
    /// (`editor.set_viewport { fog }`; audit gap FW-1).
    VIEW_SHOW_FOG = ("viewport.show_fog", "Fog of war", "viewport.toolbar", Method("editor.set_viewport"));
    /// Import dialog → a glTF mesh's Import toggle (import or skip the node's mesh, ADR-8376).
    IMPORT_DIALOG_MESH_IMPORT = ("import_dialog.mesh_import", "Import mesh", "import_dialog", Method("editor.import_with"));
    /// Frame Debugger → Previous draw (ADR-8224: the selected draw steps back and its entity is selected).
    FRAME_DRAW_PREVIOUS = ("frame_debugger.previous_draw", "Previous draw", "frame_debugger", Method("editor.frame_draw"));
    /// Frame Debugger → Next draw (ADR-8224: the selected draw steps forward and its entity is selected).
    FRAME_DRAW_NEXT = ("frame_debugger.next_draw", "Next draw", "frame_debugger", Method("editor.frame_draw"));
    /// Frame Debugger → Capture play window (ADR-8224: shown while an in-process play window is open).
    FRAME_CAPTURE_PLAY_WINDOW = ("frame_debugger.capture_play_window", "Capture play window", "frame_debugger", Method("editor.frame_capture"));
    /// Viewport → Show → Lockstep: the deterministic-index debug draw of play (ADR-8562).
    VIEW_SHOW_LOCKSTEP = ("viewport.show_lockstep", "Lockstep", "viewport.toolbar", Method("editor.set_viewport"));
    /// Profiler → Lockstep → Grid: draw the grid bounds and cells in use (ADR-8562).
    PROFILER_LOCKSTEP_GRID = ("profiler.lockstep_grid", "Grid", "profiler", Method("sim.det_debug_draw"));
    /// Profiler → Lockstep → Occupancy: draw each cell's body count.
    PROFILER_LOCKSTEP_OCCUPANCY = ("profiler.lockstep_occupancy", "Occupancy", "profiler", Method("sim.det_debug_draw"));
    /// Profiler → Lockstep → Candidates: draw the last pass's candidate pairs.
    PROFILER_LOCKSTEP_CANDIDATES = ("profiler.lockstep_candidates", "Candidates", "profiler", Method("sim.det_debug_draw"));
    /// Profiler → Lockstep → Contacts: draw the last pass's contacts.
    PROFILER_LOCKSTEP_CONTACTS = ("profiler.lockstep_contacts", "Contacts", "profiler", Method("sim.det_debug_draw"));
    /// Profiler → Lockstep → Queries: draw the last queries' shapes and hits.
    PROFILER_LOCKSTEP_QUERIES = ("profiler.lockstep_queries", "Queries", "profiler", Method("sim.det_debug_draw"));
    /// Prefab banner → Open graph (the logic graph the prefab references, P-165).
    PREFAB_LOGIC_OPEN = ("prefab.logic_open", "Open graph", "shell.prefab", Method("editor.prefab_logic"));
    /// Prefab banner → New graph for this prefab (creates it next to the prefab and attaches it, P-165).
    PREFAB_LOGIC_NEW = ("prefab.logic_new", "New graph for this prefab", "shell.prefab", Method("editor.prefab_logic"));
    /// Viewport toolbar → Show → Behavior tree (`editor.set_viewport { ai_bt }`, ADR-8463).
    VIEW_SHOW_AI_BT = ("viewport.show_ai_bt", "Behavior tree", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Show → State tree (`editor.set_viewport { ai_state_tree }`, ADR-8463).
    VIEW_SHOW_AI_STATE_TREE = ("viewport.show_ai_state_tree", "State tree", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Show → GOAP (`editor.set_viewport { ai_goap }`, ADR-8463).
    VIEW_SHOW_AI_GOAP = ("viewport.show_ai_goap", "GOAP", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Show → Squad (`editor.set_viewport { ai_squad }`, ADR-8463).
    VIEW_SHOW_AI_SQUAD = ("viewport.show_ai_squad", "Squad", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Show → Formation (`editor.set_viewport { ai_formation }`, ADR-8463).
    VIEW_SHOW_AI_FORMATION = ("viewport.show_ai_formation", "Formation", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Show → Move target (`editor.set_viewport { ai_move_to }`, ADR-8463).
    VIEW_SHOW_AI_MOVE_TO = ("viewport.show_ai_move_to", "Move target", "viewport.toolbar", Method("editor.set_viewport"));
    /// Viewport toolbar → Show → Blackboard (`editor.set_viewport { ai_blackboard }`, ADR-8463).
    VIEW_SHOW_AI_BLACKBOARD = ("viewport.show_ai_blackboard", "Blackboard", "viewport.toolbar", Method("editor.set_viewport"));
    /// World Settings → Game Rules → Edit rules… (`editor.rules_open`, ADR-8738).
    MATCH_RULES_OPEN_PANEL = ("world_settings.match_rules", "Edit rules…", "world_settings", Method("editor.rules_open"));
}

/// The registered action called `id`.
#[must_use]
pub fn find(id: &str) -> Option<&'static EditorAction> {
    ACTIONS.iter().find(|action| action.id == id)
}

/// One registered action a menu drew while [`record_menus`] ran (ADR-7496: the menu drift test
/// compares these with `crate::menus::menu_tree`, what `editor.menu_list` answers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawnItem {
    /// The menu path it was drawn under (`File`, `Play/Players`).
    pub menu: String,
    /// The registered action id.
    pub action: &'static str,
    /// The text it showed.
    pub text: String,
}

#[derive(Debug, Default)]
struct MenuRecorder {
    path: Vec<String>,
    items: Vec<DrawnItem>,
}

thread_local! {
    static MENU_RECORDER: std::cell::RefCell<Option<MenuRecorder>> =
        const { std::cell::RefCell::new(None) };
}

fn recording_menus() -> bool {
    MENU_RECORDER.with(|recorder| recorder.borrow().is_some())
}

/// Notes a registered action drawn inside a [`menu`] while [`record_menus`] runs.
fn note_drawn(action: &EditorAction, text: &str) {
    MENU_RECORDER.with(|recorder| {
        if let Some(recorder) = recorder.borrow_mut().as_mut()
            && !recorder.path.is_empty()
        {
            let menu = recorder.path.join("/");
            recorder.items.push(DrawnItem {
                menu,
                action: action.id,
                text: text.to_string(),
            });
        }
    });
}

/// Runs `draw` (one or more UI passes on this thread) and answers every registered action the
/// [`menu`]s drew, in draw order. While it runs, a menu draws its contents in place of its closed
/// popup, so every item of every menu and submenu is seen.
pub fn record_menus<R>(draw: impl FnOnce() -> R) -> (R, Vec<DrawnItem>) {
    MENU_RECORDER.with(|recorder| *recorder.borrow_mut() = Some(MenuRecorder::default()));
    let out = draw();
    let items = MENU_RECORDER
        .with(|recorder| recorder.borrow_mut().take())
        .map(|recorder| recorder.items)
        .unwrap_or_default();
    (out, items)
}

/// A menu-bar menu or a submenu (`ui.menu_button`). Items drawn inside it through the `action_*`
/// helpers are what [`record_menus`] sees under `title`.
pub fn menu<R>(ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> Option<R> {
    if !recording_menus() {
        return ui.menu_button(title, add_contents).inner;
    }
    MENU_RECORDER.with(|recorder| {
        if let Some(recorder) = recorder.borrow_mut().as_mut() {
            recorder.path.push(title.to_string());
        }
    });
    let out = ui.vertical(add_contents).inner;
    MENU_RECORDER.with(|recorder| {
        if let Some(recorder) = recorder.borrow_mut().as_mut() {
            recorder.path.pop();
        }
    });
    Some(out)
}

/// Notes a registered action drawn by a widget that is not a button (a drag grip), so the menu
/// recorder lists it like the buttons.
pub fn action_note(action: &EditorAction) {
    note_drawn(action, action.label);
}

/// A button for a registered action, labeled with its registry label.
pub fn action_button(ui: &mut egui::Ui, action: &EditorAction) -> egui::Response {
    note_drawn(action, action.label);
    ui.button(action.label)
}

/// A menu item for a registered action (a button inside a `menu_button`).
pub fn action_menu_item(ui: &mut egui::Ui, action: &EditorAction) -> egui::Response {
    note_drawn(action, action.label);
    let tip = crate::tooltips::menu_tooltip(ui, action);
    crate::tooltips::attach(ui.button(action.label), &tip)
}

/// A button for a registered action whose label carries live state (`Save level *`,
/// `Undo — Move`). The registry label stays the action's name.
pub fn action_button_text(
    ui: &mut egui::Ui,
    action: &EditorAction,
    text: impl Into<egui::WidgetText>,
) -> egui::Response {
    let text = text.into();
    note_drawn(action, text.text());
    ui.button(text)
}

/// A button for a registered action that is greyed out unless `enabled`; `text` overrides the
/// label when given.
pub fn action_enabled(
    ui: &mut egui::Ui,
    enabled: bool,
    action: &EditorAction,
    text: Option<String>,
) -> egui::Response {
    let text = text.unwrap_or_else(|| action.label.to_owned());
    note_drawn(action, &text);
    ui.add_enabled(enabled, egui::Button::new(text))
}

/// A selectable menu entry for a registered action (`selectable_label`: the active choice of a
/// group draws selected), labeled `text` (ADR-7838: View ▸ Color Vision).
pub fn action_selectable(
    ui: &mut egui::Ui,
    action: &EditorAction,
    text: &str,
    selected: bool,
) -> egui::Response {
    note_drawn(action, text);
    ui.selectable_label(selected, text)
}

/// A toggle (`toggle_value`) for a registered action.
pub fn action_toggle(ui: &mut egui::Ui, action: &EditorAction, on: &mut bool) -> egui::Response {
    note_drawn(action, action.label);
    ui.toggle_value(on, action.label)
}

/// A checkbox without visible text for a registered action, named `spoken` (the row's label
/// beside it says what it enables).
pub fn action_bare_checkbox(
    ui: &mut egui::Ui,
    action: &EditorAction,
    on: &mut bool,
    spoken: &str,
) -> egui::Response {
    note_drawn(action, spoken);
    crate::a11y_audit::bare_checkbox(ui, on, spoken)
}

// ---------------------------------------------------------------------------
// Allowlist
// ---------------------------------------------------------------------------

/// The checked-in allowlist, relative to the crate root.
pub const ALLOWLIST_FILE: &str = "agent-parity-allow.ron";

/// The allowlist as compiled into this build (what `editor.actions` reports).
const ALLOWLIST_TEXT: &str = include_str!("../agent-parity-allow.ron");

/// The class of an allowlisted UI site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SiteClass {
    /// An agent method (`via`) does what the control does.
    Method,
    /// An `editor.command` palette id (`via`) does what the control does.
    Command,
    /// Scroll, collapse, hover, tab or layout only.
    ViewOnly,
    /// Opens an OS file dialog; the agent calls `via` with the path.
    OsDialog,
    /// A confirmation dialog; `via` names the method whose parameters state the choice.
    Confirm,
    /// Changes state and has no agent path yet (a follow-up item).
    Gap,
}

impl SiteClass {
    /// The snake-case name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Method => "method",
            Self::Command => "command",
            Self::ViewOnly => "view_only",
            Self::OsDialog => "os_dialog",
            Self::Confirm => "confirm",
            Self::Gap => "gap",
        }
    }

    /// Whether the class requires a `via` (a `confirm` row names the method whose parameters
    /// state the choice, ADR-7747).
    #[must_use]
    pub fn needs_via(self) -> bool {
        matches!(
            self,
            Self::Method | Self::Command | Self::OsDialog | Self::Confirm
        )
    }
}

/// One allowlisted UI site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowEntry {
    /// Source file, relative to `crates/regia_editor/src`, `/`-separated.
    pub file: String,
    /// The line when the list was last written (informational: matching uses `text`).
    pub line: usize,
    /// Which control: `button`, `small_button`, `menu_button`, `selectable_label`, `checkbox`,
    /// `Button::new`, `ImageButton::new`, `radio_value`, `toggle_value`, `glyph_button` or
    /// `bare_checkbox` (the `a11y_audit` helpers), or a value editor ([`VALUE_CALLS`]).
    pub call: String,
    /// The trimmed source line, the site's key.
    pub text: String,
    /// The class.
    pub class: SiteClass,
    /// The method or command id (empty when the class needs none).
    #[serde(default)]
    pub via: String,
    /// Why: required on a value-editor row of class `view_only` or `gap` (ADR-7747), optional
    /// elsewhere. Left out of the file when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

impl AllowEntry {
    /// The control's label: the first string literal after the call, when there is one.
    #[must_use]
    pub fn label(&self) -> String {
        site_label(&self.text, &self.call)
    }
}

/// Parses allowlist text.
pub fn parse_allowlist(text: &str) -> Result<Vec<AllowEntry>, String> {
    ron::from_str(text).map_err(|err| format!("{ALLOWLIST_FILE}: {err}"))
}

/// The allowlist compiled into this build.
pub fn allowlist() -> Result<Vec<AllowEntry>, String> {
    parse_allowlist(ALLOWLIST_TEXT)
}

/// Writes allowlist text: a header, then one entry per line (so lanes append and diff cleanly).
#[must_use]
pub fn allowlist_text(entries: &[AllowEntry]) -> String {
    let mut out = String::from(
        "// Editor agent parity allowlist (ADR-7307, crate docs of `regia_editor::agent_parity`).\n\
         // Every `.button(`, `.small_button(`, `menu_button(`, `selectable_label(`, `.checkbox(`, `Button::new(`,\n\
         // `ImageButton::new(`, `radio_value(`, `toggle_value(`, `glyph_button(` and `bare_checkbox(` site of\n\
         // non-test editor code that is not drawn through an `action_*` helper.\n\
         // class: method | command (via = the method or editor.command id), os_dialog (via = the method\n\
         // that takes the path), confirm (via = the method whose params state the choice), view_only,\n\
         // gap (changes state, no agent path yet; the scan fails on one not in its KNOWN_GAPS list).\n\
         // `text` is the key; `line` is informational.\n\
         // Refresh: REGIA_AGENT_PARITY_UPDATE=1 cargo test -p regia_editor --test agent_parity_scan\n[\n",
    );
    let mut in_values = false;
    for entry in entries {
        if !in_values && is_value_call(&entry.call) {
            in_values = true;
            out.push_str(VALUE_SECTION_HEADER);
        }
        out.push_str("    ");
        out.push_str(
            &ron::to_string(entry).unwrap_or_else(|err| format!("/* unserializable entry: {err} */")),
        );
        out.push_str(",\n");
    }
    out.push_str("]\n");
    out
}

/// The comment that opens the allowlist's value-editor section.
const VALUE_SECTION_HEADER: &str = concat!(
    "    // Value editors (ADR-7747): `DragValue::new(`, `ComboBox::`, `selectable_value(`, `Slider::new(`,\n",
    "    // `TextEdit::singleline(`/`multiline(`, `text_edit_singleline(`/`multiline(`, `drag_angle(`,\n",
    "    // `color_edit_button`. method/command: the route that sets the same value; view_only and gap\n",
    "    // rows carry a `note` (why no route is needed; what is missing and the follow-up). Reflect-driven\n",
    "    // Inspector widgets: method via editor.set_property.\n",
);

// ---------------------------------------------------------------------------
// Source scan
// ---------------------------------------------------------------------------

/// The value-editor calls the scan looks for (ADR-7747), as [`AllowEntry::call`] names them.
pub const VALUE_CALLS: &[&str] = &[
    "DragValue::new",
    "ComboBox",
    "selectable_value",
    "Slider::new",
    "TextEdit::singleline",
    "TextEdit::multiline",
    "text_edit_singleline",
    "text_edit_multiline",
    "drag_angle",
    "color_edit_button",
];

/// Whether `call` names a value editor (one of [`VALUE_CALLS`]) rather than a button.
#[must_use]
pub fn is_value_call(call: &str) -> bool {
    VALUE_CALLS.contains(&call)
}

/// The controls the scan looks for: (pattern, call name, whether the byte before the pattern must
/// not be an identifier character).
const PATTERNS: &[(&str, &str, bool)] = &[
    (".button(", "button", false),
    ("menu_button(", "menu_button", true),
    ("selectable_label(", "selectable_label", true),
    (".checkbox(", "checkbox", false),
    ("Button::new(", "Button::new", true),
    ("toggle_value(", "toggle_value", true),
    (".small_button(", "small_button", false),
    ("radio_value(", "radio_value", true),
    // A bare radio button and a built checkbox widget (review sprint-w11-rev-e-k #3).
    (".radio(", "radio", false),
    ("Checkbox::new(", "Checkbox::new", true),
    ("ImageButton::new(", "ImageButton::new", true),
    // The named-glyph helpers of `a11y_audit` (ADR-7452): their call sites are the controls.
    ("glyph_button(", "glyph_button", true),
    ("bare_checkbox(", "bare_checkbox", true),
    // Value editors (ADR-7747, [`VALUE_CALLS`]).
    ("DragValue::new(", "DragValue::new", true),
    ("ComboBox::", "ComboBox", true),
    ("selectable_value(", "selectable_value", true),
    ("Slider::new(", "Slider::new", true),
    ("TextEdit::singleline(", "TextEdit::singleline", true),
    ("TextEdit::multiline(", "TextEdit::multiline", true),
    ("text_edit_singleline(", "text_edit_singleline", true),
    ("text_edit_multiline(", "text_edit_multiline", true),
    ("drag_angle(", "drag_angle", true),
    ("color_edit_button", "color_edit_button", true),
];

/// Files the scan skips: the helpers themselves.
const EXEMPT_FILES: &[&str] = &["agent_parity.rs"];

/// One UI control site in non-test source.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Site {
    /// File relative to the scanned `src`, `/`-separated.
    pub file: String,
    /// 1-based line.
    pub line: usize,
    /// The control (see [`AllowEntry::call`]).
    pub call: String,
    /// The trimmed source line.
    pub text: String,
}

/// Every UI control site of the non-test `.rs` files under `src` (an editor crate's `src`
/// directory). Test code is left out: items behind `#[cfg(test)]`, files whose first inner
/// attribute is `#![cfg(test)]`, and files pulled in by a `#[cfg(test)] mod name;` (and their
/// submodules).
pub fn scan_sites(src: &Path) -> std::io::Result<Vec<Site>> {
    let mut sites = Vec::new();
    for source in non_test_sources(src)? {
        if EXEMPT_FILES.contains(&source.file.as_str()) {
            continue;
        }
        let (rel, text, masked) = (&source.file, &source.text, &source.masked);
        let lines: Vec<&str> = text.lines().collect();
        let mut found: Vec<(usize, &str)> = Vec::new();
        for &(pattern, call, boundary) in PATTERNS {
            let mut i = 0;
            while let Some(rel_at) = masked[i..].find(pattern) {
                let at = i + rel_at;
                i = at + pattern.len();
                if boundary && at > 0 {
                    let before = masked.as_bytes()[at - 1];
                    if before.is_ascii_alphanumeric() || before == b'_' {
                        continue;
                    }
                }
                // A helper's own definition (`fn glyph_button(`) is not a call site.
                let head = masked[..at].trim_end();
                if head.ends_with("fn")
                    && !head[..head.len() - 2]
                        .bytes()
                        .next_back()
                        .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    continue;
                }
                found.push((at, call));
            }
        }
        found.sort_unstable();
        for (at, call) in found {
            let line = masked[..at].bytes().filter(|b| *b == b'\n').count();
            sites.push(Site {
                file: rel.clone(),
                line: line + 1,
                call: call.to_owned(),
                text: lines.get(line).map_or_else(String::new, |l| l.trim().to_owned()),
            });
        }
    }
    sites.sort();
    Ok(sites)
}

/// One non-test `.rs` file of an editor crate's `src`, as [`scan_sites`] and the UI text scan
/// (`tests/ui_text_scan.rs`, ADR-8586) read it.
#[derive(Debug, Clone)]
pub struct SourceFile {
    /// File relative to the scanned `src`, `/`-separated.
    pub file: String,
    /// The file's text.
    pub text: String,
    /// [`mask`]ed text with every `#[cfg(test)]` item blanked too (byte offsets and newlines kept),
    /// so a search over it only sees non-test code.
    pub masked: String,
}

/// The non-test `.rs` files under `src` (an editor crate's `src` directory), sorted by path. Test
/// code is left out: files whose first inner attribute is `#![cfg(test)]` and files pulled in by a
/// `#[cfg(test)] mod name;` (and their submodules) are skipped, and items behind `#[cfg(test)]` are
/// blanked in [`SourceFile::masked`].
pub fn non_test_sources(src: &Path) -> std::io::Result<Vec<SourceFile>> {
    let mut files = Vec::new();
    walk(src, &mut files)?;
    files.sort();
    let mut texts = BTreeMap::new();
    for path in &files {
        texts.insert(path.clone(), std::fs::read_to_string(path)?);
    }
    // Files that are test code as a whole.
    let mut test_only: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    let mut pending: Vec<PathBuf> = Vec::new();
    for (path, text) in &texts {
        let masked = mask(text);
        if inner_cfg_test(&masked) {
            pending.push(path.clone());
        }
        for (start, end) in cfg_test_spans(&masked) {
            if let Some(name) = external_mod(&masked[start..end]) {
                pending.extend(resolve_mod(path, &name));
            }
        }
    }
    while let Some(path) = pending.pop() {
        if !test_only.insert(path.clone()) {
            continue;
        }
        if let Some(text) = texts.get(&path) {
            let masked = mask(text);
            let mut i = 0;
            while let Some(rel) = masked[i..].find("mod ") {
                let at = i + rel;
                if let Some(name) = external_mod(&masked[at..]) {
                    pending.extend(resolve_mod(&path, &name));
                }
                i = at + 4;
            }
        }
    }
    let mut out = Vec::new();
    for (path, text) in texts {
        if test_only.contains(&path) {
            continue;
        }
        let file = path
            .strip_prefix(src)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let mut masked = mask(&text).into_bytes();
        for (start, end) in cfg_test_spans(&String::from_utf8_lossy(&masked)) {
            for byte in &mut masked[start..end] {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
        }
        let masked = String::from_utf8_lossy(&masked).into_owned();
        out.push(SourceFile { file, text, masked });
    }
    Ok(out)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            walk(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// `text` with comments and the insides of string and char literals replaced by spaces (newlines
/// and byte offsets kept), so pattern searches only see code.
#[must_use]
pub fn mask(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = bytes.to_vec();
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for byte in &mut out[from..to.min(bytes.len())] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    };
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            let end = text[i..].find('\n').map_or(bytes.len(), |rel| i + rel);
            blank(&mut out, i, end);
            i = end;
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let mut depth = 1;
            let mut j = i + 2;
            while j < bytes.len() && depth > 0 {
                if bytes[j] == b'/' && bytes.get(j + 1) == Some(&b'*') {
                    depth += 1;
                    j += 2;
                } else if bytes[j] == b'*' && bytes.get(j + 1) == Some(&b'/') {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            blank(&mut out, i, j);
            i = j;
            continue;
        }
        // Raw strings: r"..", r#".."#, br"..".
        let prev_ident = i > 0 && ident(bytes[i - 1]);
        if !prev_ident && (b == b'r' || (b == b'b' && bytes.get(i + 1) == Some(&b'r'))) {
            let mut j = if b == b'b' { i + 2 } else { i + 1 };
            let mut hashes = 0;
            while bytes.get(j) == Some(&b'#') {
                hashes += 1;
                j += 1;
            }
            if bytes.get(j) == Some(&b'"') {
                let closer = format!("\"{}", "#".repeat(hashes));
                let body = j + 1;
                let end = text[body..].find(&closer).map_or(bytes.len(), |rel| body + rel);
                blank(&mut out, body, end);
                i = end + closer.len();
                continue;
            }
        }
        if b == b'"' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'"' {
                j += if bytes[j] == b'\\' { 2 } else { 1 };
            }
            blank(&mut out, i + 1, j);
            i = j + 1;
            continue;
        }
        if b == b'\'' && !prev_ident {
            // A char literal ('x', '\n', '\u{..}', a multi-byte char) or a lifetime ('a).
            if bytes.get(i + 1) == Some(&b'\\') && i + 3 <= bytes.len() {
                let end = text[i + 3..].find('\'').map_or(bytes.len(), |rel| i + 3 + rel);
                blank(&mut out, i + 1, end);
                i = end + 1;
                continue;
            }
            if let Some(ch) = text[i + 1..].chars().next() {
                let after = i + 1 + ch.len_utf8();
                if bytes.get(after) == Some(&b'\'') {
                    blank(&mut out, i + 1, after);
                    i = after + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn inner_cfg_test(masked: &str) -> bool {
    let mut rest = masked.trim_start();
    while rest.starts_with("#![") {
        let Some(close) = attr_close(rest.as_bytes(), 2) else {
            return false;
        };
        let body: String = rest[3..close].split_whitespace().collect();
        if body == "cfg(test)" {
            return true;
        }
        rest = rest[close + 1..].trim_start();
    }
    false
}

/// Byte ranges of the items behind a `#[cfg(test)]` attribute, attribute included.
fn cfg_test_spans(masked: &str) -> Vec<(usize, usize)> {
    let bytes = masked.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while let Some(rel) = masked[i..].find("#[") {
        let at = i + rel;
        let Some(close) = attr_close(bytes, at + 1) else {
            break;
        };
        let body: String = masked[at + 2..close].split_whitespace().collect();
        if body != "cfg(test)" {
            i = close + 1;
            continue;
        }
        // Skip further attributes, then take the item to its closing brace or `;`.
        let mut j = close + 1;
        loop {
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if bytes.get(j) == Some(&b'#') && bytes.get(j + 1) == Some(&b'[') {
                match attr_close(bytes, j + 1) {
                    Some(end) => j = end + 1,
                    None => break,
                }
            } else {
                break;
            }
        }
        let mut depth = 0i32;
        let mut end = bytes.len();
        while j < bytes.len() {
            match bytes[j] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = j + 1;
                        break;
                    }
                }
                b';' if depth == 0 => {
                    end = j + 1;
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        spans.push((at, end));
        i = end;
    }
    spans
}

/// The index of the `]` closing the attribute whose `[` is at `open`.
fn attr_close(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (k, byte) in bytes.iter().enumerate().skip(open) {
        match byte {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
    }
    None
}

/// `name` when `item` (attributes and visibility allowed before it) is `mod name;`.
fn external_mod(item: &str) -> Option<String> {
    let mut rest = item.trim_start();
    while rest.starts_with("#[") {
        let close = attr_close(rest.as_bytes(), 1)?;
        rest = rest[close + 1..].trim_start();
    }
    if let Some(after) = rest.strip_prefix("pub") {
        rest = after.trim_start();
        if rest.starts_with('(') {
            rest = rest[rest.find(')')? + 1..].trim_start();
        }
    }
    let after = rest.strip_prefix("mod ")?.trim_start();
    let end = after
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(after.len());
    let name = &after[..end];
    (!name.is_empty() && after[end..].trim_start().starts_with(';')).then(|| name.to_owned())
}

fn resolve_mod(parent: &Path, name: &str) -> Vec<PathBuf> {
    let stem = parent.file_stem().and_then(|stem| stem.to_str());
    let dir = if matches!(stem, Some("mod" | "lib" | "main")) {
        parent.parent().map(Path::to_path_buf)
    } else {
        Some(parent.with_extension(""))
    };
    let Some(dir) = dir else {
        return Vec::new();
    };
    [dir.join(format!("{name}.rs")), dir.join(name).join("mod.rs")]
        .into_iter()
        .filter(|path| path.is_file())
        .collect()
}

/// The first string literal after `call` on `text`, or empty.
fn site_label(text: &str, call: &str) -> String {
    let Some(at) = text.find(call) else {
        return String::new();
    };
    let rest = &text[at + call.len()..];
    let Some(open) = rest.find('"') else {
        return String::new();
    };
    let body = &rest[open + 1..];
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '"' => break,
            _ => out.push(c),
        }
    }
    out
}

/// What [`parity_report`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParityReport {
    /// Sites in the source that the allowlist does not list.
    pub unlisted: Vec<Site>,
    /// Allowlist entries no source site matches.
    pub stale: Vec<AllowEntry>,
    /// Entries whose class needs a `via` and has none, or whose class forbids one.
    pub malformed: Vec<AllowEntry>,
    /// Value-editor entries of class `view_only` or `gap` with no `note` (ADR-7747).
    pub unreasoned: Vec<AllowEntry>,
    /// The scanned sites matched to their entries (index into the allowlist), in site order.
    pub matched: Vec<(Site, usize)>,
}

impl ParityReport {
    /// Whether the source and the allowlist agree.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.unlisted.is_empty()
            && self.stale.is_empty()
            && self.malformed.is_empty()
            && self.unreasoned.is_empty()
    }
}

/// Matches scanned `sites` against `allow` by (file, text), as a multiset.
#[must_use]
pub fn parity_report(sites: &[Site], allow: &[AllowEntry]) -> ParityReport {
    let mut report = ParityReport::default();
    let mut used = vec![false; allow.len()];
    for site in sites {
        // Prefer the entry with the same line, then any unused one with the same key.
        let candidates: Vec<usize> = (0..allow.len())
            .filter(|&k| !used[k] && allow[k].file == site.file && allow[k].text == site.text)
            .collect();
        let pick = candidates
            .iter()
            .copied()
            .find(|&k| allow[k].line == site.line)
            .or_else(|| candidates.first().copied());
        match pick {
            Some(k) => {
                used[k] = true;
                report.matched.push((site.clone(), k));
            }
            None => report.unlisted.push(site.clone()),
        }
    }
    for (k, entry) in allow.iter().enumerate() {
        if !used[k] {
            report.stale.push(entry.clone());
        }
        let has_via = !entry.via.trim().is_empty();
        if entry.class.needs_via() != has_via {
            report.malformed.push(entry.clone());
        }
        if is_value_call(&entry.call)
            && matches!(entry.class, SiteClass::ViewOnly | SiteClass::Gap)
            && entry.note.trim().is_empty()
        {
            report.unreasoned.push(entry.clone());
        }
    }
    report
}

/// The allowlist rewritten for `sites`: matched entries keep their class, `via` and `note` (with
/// the line refreshed), new sites are added as `gap`, vanished entries are dropped. Button rows
/// come first, then the value-editor section ([`is_value_call`]).
#[must_use]
pub fn refreshed_allowlist(sites: &[Site], allow: &[AllowEntry]) -> Vec<AllowEntry> {
    let report = parity_report(sites, allow);
    let mut out: Vec<AllowEntry> = report
        .matched
        .iter()
        .map(|(site, k)| AllowEntry {
            line: site.line,
            ..allow[*k].clone()
        })
        .collect();
    out.extend(report.unlisted.iter().map(|site| AllowEntry {
        file: site.file.clone(),
        line: site.line,
        call: site.call.clone(),
        text: site.text.clone(),
        class: SiteClass::Gap,
        via: String::new(),
        note: String::new(),
    }));
    out.sort_by(|a, b| {
        is_value_call(&a.call)
            .cmp(&is_value_call(&b.call))
            .then(a.file.cmp(&b.file))
            .then(a.line.cmp(&b.line))
            .then(a.text.cmp(&b.text))
    });
    out
}

// ---------------------------------------------------------------------------
// `editor.actions`
// ---------------------------------------------------------------------------

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

const CLASSES: &[&str] = &["method", "command", "view_only", "os_dialog", "confirm", "gap"];

fn action_error(code: &'static str, message: String) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        code,
        message,
        "editor.actions takes an optional panel (string) and class (method | command | view_only | os_dialog | confirm | gap)",
    )
}

/// `editor.actions { panel?, class? }`: the action registry and the classified allowlist.
fn editor_actions(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let panel = match params.get("panel") {
        None | Some(Value::Null) => None,
        Some(Value::String(panel)) => Some(panel.as_str()),
        Some(other) => {
            return Err(action_error(
                "bad_panel",
                format!("panel must be a string, got {other}"),
            ));
        }
    };
    let class = match params.get("class") {
        None | Some(Value::Null) => None,
        Some(Value::String(class)) if CLASSES.contains(&class.as_str()) => Some(class.as_str()),
        Some(other) => {
            return Err(action_error(
                "bad_class",
                format!("class must be one of {}, got {other}", CLASSES.join(", ")),
            ));
        }
    };
    let allow = allowlist().map_err(|err| action_error("allowlist_unreadable", err))?;
    let panel_matches = |name: &str| {
        panel.is_none_or(|want| {
            name == want || name.starts_with(&format!("{want}.")) || name.starts_with(&format!("{want}/"))
        })
    };
    let actions: Vec<Value> = ACTIONS
        .iter()
        .filter(|action| panel_matches(action.panel))
        .filter(|action| class.is_none_or(|want| want == action.path.class()))
        .map(|action| {
            let mut row = json!({
                "id": action.id,
                "label": action.label,
                "panel": action.panel,
                "class": action.path.class(),
            });
            match action.path {
                ActionPath::Method(id) | ActionPath::OsDialog(id) | ActionPath::Confirm(id) => {
                    row["method"] = json!(id);
                }
                ActionPath::Command(id) => row["command"] = json!(id),
                ActionPath::ViewOnly => {}
            }
            row
        })
        .collect();
    let mut counts: BTreeMap<&str, usize> = CLASSES.iter().map(|c| (*c, 0)).collect();
    let sites: Vec<Value> = allow
        .iter()
        .filter(|entry| panel_matches(entry.file.trim_end_matches(".rs")))
        .filter(|entry| class.is_none_or(|want| want == entry.class.name()))
        .map(|entry| {
            *counts.entry(entry.class.name()).or_default() += 1;
            let mut row = json!({
                "panel": entry.file.trim_end_matches(".rs"),
                "file": format!("crates/regia_editor/src/{}", entry.file),
                "line": entry.line,
                "call": entry.call,
                "label": entry.label(),
                "class": entry.class.name(),
            });
            if !entry.note.is_empty() {
                row["note"] = json!(entry.note);
            }
            match entry.class {
                SiteClass::Command => row["command"] = json!(entry.via),
                SiteClass::Method | SiteClass::OsDialog | SiteClass::Confirm if !entry.via.is_empty() => {
                    row["method"] = json!(entry.via);
                }
                _ => {}
            }
            row
        })
        .collect();
    Ok(json!({
        "actions": actions,
        "sites": sites,
        "counts": counts,
    }))
}

/// `editor.prefab_override { entity, component, path, action }`: the Inspector's per-override
/// Revert and Apply buttons (ADR-7307).
fn editor_prefab_override(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bad = |code: &str, message: String| {
        RpcError::new(
            INVALID_PARAMS,
            code,
            message,
            "editor.prefab_override takes entity (number), component (string), path (string) and action (\"revert\" | \"apply\")",
        )
    };
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad("bad_entity", "`entity` must be an entity id".into()))?;
    let entity = Entity::try_from_bits(bits)
        .ok_or_else(|| bad("bad_entity", format!("{bits} is not an entity id")))?;
    let text = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| bad("bad_params", format!("`{key}` must be a string")))
    };
    let component = text("component")?;
    let path = text("path")?;
    let action = text("action")?;
    let result = match action.as_str() {
        "revert" => crate::inspector::multi::revert_prefab_override(world, entity, &component, &path),
        "apply" => crate::inspector::multi::apply_prefab_override(world, entity, &component, &path),
        other => {
            return Err(bad(
                "bad_action",
                format!("action must be \"revert\" or \"apply\", got {other:?}"),
            ));
        }
    };
    match result {
        Ok(report) => Ok(json!({
            "action": action,
            "entity": bits,
            "component": component,
            "path": path,
            "edited": report.edited,
            "undo_depth": report.undo_depth,
        })),
        Err(err) => Err(RpcError::new(
            INVALID_PARAMS,
            err.code,
            err.message,
            "list an instance's overrides with scene.get_entity (PrefabInstance overrides)",
        )),
    }
}

/// The `editor.actions` and `editor.prefab_override` methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.prefab_override",
                tool: "editor_prefab_override",
                description: "The Inspector's Revert and Apply of one prefab override on an instance: \
                    revert writes the template value back to the instance; apply writes the instance's \
                    live field into the loaded template (not the prefab file). One undo step. Errors: \
                    bad_entity, bad_params, bad_action, not_prefab_instance, override_absent, \
                    prefab_template_missing, path_unknown, feed_owned, prefab_apply_failed.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: true,
                        description: "The instance entity that carries the override.",
                    },
                    ParamSchema {
                        name: "component",
                        kind: "string",
                        required: true,
                        description: "The overridden component (short or full type name).",
                    },
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "The overridden field path (as the override lists it).",
                    },
                    ParamSchema {
                        name: "action",
                        kind: "string",
                        required: true,
                        description: "\"revert\" or \"apply\".",
                    },
                ],
                result: "{ action, entity, component, path, edited, undo_depth }",
                mutating: true,
                permission: Permission::EditScene,
            },
            editor_prefab_override as Handler,
        ),
        (
            MethodSchema {
                name: "editor.actions",
                tool: "editor_actions",
                description: "List the editor's UI actions and how an agent performs each: the action \
                registry (id, label, panel, and the agent method or editor.command id) and every \
                other button, menu item, toggle and checkbox site, and every value editor (drag \
                value, combo box, selectable value, slider, text edit, angle and color edit), with \
                its class (method, command, view_only, os_dialog, confirm or gap) and, where given, \
                the note that says why. Errors: bad_panel, bad_class, allowlist_unreadable.",
                params: &[
                    ParamSchema {
                        name: "panel",
                        kind: "string",
                        required: false,
                        description: "Only this panel: a registry panel (`shell`, `shell.file`) or a source \
                        file stem (`ui_designer_panel`, `procgen/panel`).",
                    },
                    ParamSchema {
                        name: "class",
                        kind: "string",
                        required: false,
                        description: "Only this class: method | command | view_only | os_dialog | confirm | gap.",
                    },
                ],
                result: "{ actions: [{id, label, panel, class, method?, command?}], sites: [{panel, file, \
                line, call, label, class, method?, command?, note?}], counts: {class: n} }",
                mutating: false,
                permission: Permission::Read,
            },
            editor_actions as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_hides_comments_and_strings_but_keeps_offsets() {
        let src = "let a = \"ui.button(\"; // ui.button(\nui.button(\"x\"); /* .checkbox( */ 'b';\n";
        let masked = mask(src);
        assert_eq!(masked.len(), src.len());
        assert_eq!(masked.matches(".button(").count(), 1, "{masked}");
        assert!(!masked.contains(".checkbox("), "{masked}");
    }

    #[test]
    fn cfg_test_items_and_test_files_are_skipped() {
        let dir = std::env::temp_dir().join(format!("regia-parity-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("lib")).expect("dir");
        std::fs::write(
            dir.join("lib.rs"),
            "fn ui(ui: &mut egui::Ui) { ui.button(\"Raw\"); }\n#[cfg(test)]\nmod tests;\n#[cfg(test)]\nmod inner { fn t(ui: &mut egui::Ui) { ui.button(\"T\"); } }\n",
        )
        .expect("write");
        std::fs::write(
            dir.join("tests.rs"),
            "fn t(ui: &mut egui::Ui) { ui.button(\"T2\"); }\n",
        )
        .expect("write");
        let sites = scan_sites(&dir).expect("scan");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(sites.len(), 1, "{sites:?}");
        assert_eq!(sites[0].text, "fn ui(ui: &mut egui::Ui) { ui.button(\"Raw\"); }");
        assert_eq!(site_label(&sites[0].text, "button"), "Raw");
    }

    #[test]
    fn report_matches_by_text_and_flags_unlisted_stale_and_malformed() {
        let site = |line, text: &str| Site {
            file: "a.rs".into(),
            line,
            call: "button".into(),
            text: text.into(),
        };
        let entry = |line, text: &str, class, via: &str| AllowEntry {
            file: "a.rs".into(),
            line,
            call: "button".into(),
            text: text.into(),
            class,
            via: via.into(),
            note: String::new(),
        };
        let sites = vec![site(3, "ui.button(\"A\")"), site(9, "ui.button(\"B\")")];
        let allow = vec![
            entry(1, "ui.button(\"A\")", SiteClass::ViewOnly, ""),
            entry(2, "ui.button(\"Gone\")", SiteClass::Gap, ""),
            entry(4, "ui.button(\"Bad\")", SiteClass::Method, ""),
        ];
        let report = parity_report(&sites, &allow);
        assert_eq!(report.unlisted, vec![site(9, "ui.button(\"B\")")]);
        assert_eq!(report.stale.len(), 2);
        assert_eq!(report.malformed.len(), 1);
        let refreshed = refreshed_allowlist(&sites, &allow);
        assert_eq!(refreshed.len(), 2);
        assert_eq!(refreshed[0].line, 3, "line refreshed");
        assert_eq!(refreshed[1].class, SiteClass::Gap, "new site is a gap");
        let text = allowlist_text(&refreshed);
        assert_eq!(parse_allowlist(&text).expect("round trip"), refreshed);
    }

    #[test]
    fn registry_ids_are_unique() {
        let mut ids: Vec<&str> = ACTIONS.iter().map(|a| a.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len());
    }
}
