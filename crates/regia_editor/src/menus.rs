// SPDX-License-Identifier: MPL-2.0
//! The editor menu tree (`docs/specs/19-editor.md` § 2, ADR-7475): File, Create, Edit, View,
//! Tools, Play and Help, each item with the agent call that does what it does.
//!
//! [`menu_tree`] is the one list `editor.menu_list` answers. The shell draws the menus through
//! `crate::agent_parity::menu` and the `action_*` helpers, and the shell test
//! `the_drawn_menus_are_the_menu_list` draws every menu open under
//! `crate::agent_parity::record_menus` and checks that the drawn items are exactly the tree's, in
//! order (ADR-7496). Each item names its agent method and the parameters that make the call do
//! what a click does, so the parity scan and an agent see every menu item's agent path.
//!
//! Tools ▸ Rebuild asset database ([`rebuild_asset_db`]) runs in process: it re-pairs moved
//! `.meta` sidecars (the asset pipeline's repair), drops database rows whose source is gone, and
//! re-lists the Content Browser index; the next cook re-imports what changed. It never waits on
//! the UI thread: while a package, Clear caches or agent cook runs it is disabled, and when
//! another process holds the cook lock it answers `tools_busy` at once. Tools ▸ Clear caches
//! asks first, then runs `regia clean --cache` as an editor job
//! ([`crate::package_job::start_clear_caches`]). The Tools menu and the confirm window draw here;
//! the menu-tree port credits the unpushed `lane/sprint-editor-menus-package-help` (ADR-3469).

use std::collections::BTreeSet;
use std::path::Path;

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::agent_parity::{self as parity, ActionPath, EditorAction};
use crate::viewport_agent::Handler;

/// One menu item and its agent path.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    /// The menu (`File`, `Tools`, ...).
    pub menu: &'static str,
    /// What the item shows.
    pub label: String,
    /// The registered UI action that draws it (`crate::agent_parity::ACTIONS`).
    pub action: &'static EditorAction,
    /// The agent method that does what the item does.
    pub method: &'static str,
    /// The parameters that make the method do what a click does.
    pub params: Value,
}

impl MenuItem {
    /// `Menu/Label`.
    #[must_use]
    pub fn path(&self) -> String {
        format!("{}/{}", self.menu, self.label)
    }

    /// The protocol object.
    #[must_use]
    pub fn to_json(&self, enabled: bool) -> Value {
        json!({
            "path": self.path(),
            "label": self.label,
            "action": self.action.id,
            "class": self.action.path.class(),
            "method": self.method,
            "params": self.params,
            "enabled": enabled,
        })
    }
}

/// The menus, in menu-bar order.
pub const MENUS: &[&str] = &[
    "File", "Create", "Edit", "View", "Tools", "Sequence", "Play", "Help", "Window",
];

/// How many Players entries the Play menu offers (`editor.play { players }` takes 1..=8).
pub const MENU_PLAYERS: u8 = 4;

/// The agent method and default parameters of an action's path.
fn agent_call(action: &EditorAction) -> (&'static str, Value) {
    match action.path {
        ActionPath::Method(method) => (method, json!({})),
        ActionPath::Command(id) => ("editor.command", json!({ "run": id })),
        ActionPath::OsDialog(method) => (method, json!({ "path": "<file>" })),
        ActionPath::Confirm(method) => (method, json!({ "confirm": true })),
        ActionPath::ViewOnly => ("", json!({})),
    }
}

fn item(menu: &'static str, action: &'static EditorAction) -> MenuItem {
    let (method, params) = agent_call(action);
    MenuItem {
        menu,
        label: action.label.to_string(),
        action,
        method,
        params,
    }
}

fn item_with(menu: &'static str, action: &'static EditorAction, label: &str, params: Value) -> MenuItem {
    let (method, _) = agent_call(action);
    MenuItem {
        menu,
        label: label.to_string(),
        action,
        method,
        params,
    }
}

/// Every menu item, in menu order.
#[must_use]
pub fn menu_tree() -> Vec<MenuItem> {
    use parity::*;
    let invoke = |path: &str| json!({ "path": path });
    let mut items = vec![
        item("File", &FILE_NEW_LEVEL),
        item("File", &FILE_OPEN_LEVEL),
        // File ▸ New level variant… (ADR-7823), drawn after Open level.
        item("File", &FILE_NEW_LEVEL_VARIANT),
        item_with(
            "File",
            &FILE_NEW_SCRIPT,
            FILE_NEW_SCRIPT.label,
            json!({ "open": true }),
        ),
        item("File", &FILE_OPEN_KIT),
        item("File", &FILE_SAVE_LEVEL),
        item("File", &FILE_SAVE_LEVEL_AS),
        item_with(
            "File",
            &FILE_SAVE_ALL,
            FILE_SAVE_ALL.label,
            json!({ "dialog": true }),
        ),
        item_with("File", &FILE_RECENT, FILE_RECENT.label, invoke("File/Recent")),
        item("File", &FILE_EXPORT_GLB),
        item_with(
            "File",
            &FILE_PACKAGE,
            FILE_PACKAGE.label,
            json!({ "target": crate::package_job::host_target(), "profile": "dev", "confirm": true }),
        ),
        // File ▸ Screenshot… (ADR-7768): opens the dialog; `editor.screenshot` captures.
        item("File", &FILE_SCREENSHOT),
        item("File", &FILE_CREATE_ENTITY),
        // File ▸ Version history… and Duplicate project… (ADR-7928).
        item("File", &FILE_VERSION_HISTORY),
        item("File", &FILE_DUPLICATE_PROJECT),
        item_with("File", &FILE_EXIT, FILE_EXIT.label, invoke("File/Exit")),
    ];
    // Create ▸ Shape draws one entry per primitive kind (`editor.command { run: "create.<kind>" }`).
    for kind in regia_render::PrimitiveShape::KINDS {
        items.push(item_with(
            "Create",
            &CREATE_SHAPE,
            &format!("{}/{}", CREATE_SHAPE.label, crate::shapes::shape_label(kind)),
            json!({ "run": format!("create.{kind}") }),
        ));
    }
    // Every other registered Create action (`editor.create_entity { kind }`, the id's suffix).
    for action in ACTIONS
        .iter()
        .filter(|action| action.panel == "shell.create" && action.id != CREATE_SHAPE.id)
    {
        let kind = action.id.strip_prefix("create.").unwrap_or(action.id);
        items.push(item_with("Create", action, action.label, json!({ "kind": kind })));
    }
    for kind in ["box", "cylinder", "wedge", "stairs", "sphere"] {
        items.push(item_with(
            "Create",
            &CREATE_BLOCKOUT,
            &format!("Blockout/{}", crate::shapes::shape_label(kind)),
            json!({"shape":kind,"op":"add"}),
        ));
    }
    items.push(item_with(
        "Create",
        &BAKE_BLOCKOUT,
        "Blockout/Bake to mesh",
        json!({"group":"<group>"}),
    ));
    items.extend([
        item("Edit", &EDIT_UNDO),
        item("Edit", &EDIT_REDO),
        item_with("Edit", &EDIT_CUT, EDIT_CUT.label, invoke("Edit/Cut")),
        item_with("Edit", &EDIT_COPY, EDIT_COPY.label, invoke("Edit/Copy")),
        item_with("Edit", &EDIT_PASTE, EDIT_PASTE.label, invoke("Edit/Paste")),
        item_with(
            "Edit",
            &EDIT_PASTE_AS_CHILD,
            EDIT_PASTE_AS_CHILD.label,
            invoke("Edit/Paste as child"),
        ),
        item_with(
            "Edit",
            &EDIT_PASTE_AT_ORIGINAL,
            EDIT_PASTE_AT_ORIGINAL.label,
            invoke("Edit/Paste at original position"),
        ),
        item_with(
            "Edit",
            &EDIT_DUPLICATE,
            EDIT_DUPLICATE.label,
            invoke("Edit/Duplicate"),
        ),
        item_with("Edit", &EDIT_DELETE, EDIT_DELETE.label, invoke("Edit/Delete")),
        item_with(
            "Edit",
            &EDIT_SELECT_ALL,
            EDIT_SELECT_ALL.label,
            invoke("Edit/Select all"),
        ),
        // Edit ▸ Select ▸ … (select similar, ADR-7769).
        item_with(
            "Edit",
            &EDIT_SIMILAR_KIND,
            "Select/All of the same kind",
            json!({ "run": "selection.similar_kind" }),
        ),
        item_with(
            "Edit",
            &EDIT_SIMILAR_COMPONENTS,
            "Select/All with the same components",
            json!({ "run": "selection.similar_components" }),
        ),
        item_with(
            "Edit",
            &EDIT_SIMILAR_ASSET,
            "Select/All using the same asset",
            json!({ "run": "selection.similar_asset" }),
        ),
        item_with(
            "Edit",
            &EDIT_SIMILAR_LAYER,
            "Select/All in the same layer",
            json!({ "run": "selection.similar_layer" }),
        ),
        item("Edit", &EDIT_PREFERENCES),
        item("Edit", &EDIT_PROJECT_SETTINGS),
        item("Edit", &EDIT_WORLD_SETTINGS),
        item("Edit", &EDIT_PREFAB_OVERRIDES),
        item("Edit", &EDIT_PREFAB_IN_PLACE),
        item("View", &VIEW_GRID),
        item("View", &VIEW_LABELS),
        item("View", &VIEW_REALTIME),
        item("View", &VIEW_FRAME_DEBUGGER),
        item("View", &VIEW_PROFILER),
        item("View", &VIEW_DIAGNOSTICS),
        item("View", &VIEW_RENDERING_DEBUG),
        // View ▸ Asset Jobs (ADR-7788).
        item("View", &VIEW_ASSET_JOBS),
        item("View", &VIEW_REVISION_CONTROL),
        item("View", &VIEW_FRAME_SELECTION),
        item("View", &VIEW_RESET_ZOOM),
        // Camera and object (ADR-7755).
        item("View", &VIEW_CAMERA_TO_SELECTION),
        item("View", &VIEW_SELECTION_TO_CAMERA),
        item("View", &VIEW_LAYOUT_SAVE),
        item("View", &VIEW_LAYOUT_SAVE_DEFAULT),
        item("View", &VIEW_LAYOUT_RESET),
    ]);
    // View ▸ Workspace, Layouts, Maximize, Full screen, Panels and Feature profile, in the order the
    // View menu draws them (ADR-7753).
    for workspace in crate::workspaces::BUILT_IN_WORKSPACES {
        items.push(item_with(
            "View",
            &VIEW_WORKSPACE,
            &format!("{}/{}", VIEW_WORKSPACE.label, workspace.title),
            json!({ "id": workspace.id }),
        ));
    }
    items.push(item_with(
        "View",
        &VIEW_NAMED_LAYOUTS,
        &format!("{}/Save", VIEW_NAMED_LAYOUTS.label),
        json!({ "name": "<name>" }),
    ));
    items.push(item("View", &VIEW_MAXIMIZE_PANEL));
    items.push(item("View", &VIEW_FULLSCREEN_VIEWPORT));
    for panel in crate::editor_layout::BUILT_IN_PANELS {
        items.push(item_with(
            "View",
            &VIEW_PANELS,
            &format!("{}/{}", VIEW_PANELS.label, panel.title()),
            json!({ "open": panel.id() }),
        ));
    }
    for profile in crate::feature_profiles::BUILT_IN_PROFILES {
        items.push(item_with(
            "View",
            &VIEW_FEATURE_PROFILE,
            &format!("{}/{profile}", VIEW_FEATURE_PROFILE.label),
            json!({ "name": profile }),
        ));
    }
    items.extend([
        // The Bulk Edit grid (ADR-7787): the editor has no Window menu; panels open from View.
        item("View", &VIEW_BULK_EDIT),
    ]);
    // View ▸ Color Vision draws one entry per filter (`editor.color_vision`, ADR-7838).
    for choice in &crate::chrome_color_vision::CHOICES {
        items.push(item_with(
            "View",
            &VIEW_COLOR_VISION,
            &format!("{}/{}", VIEW_COLOR_VISION.label, choice.label),
            choice.params(),
        ));
    }
    items.extend([
        item("Tools", &TOOLS_REBUILD_ASSET_DB),
        item("Tools", &TOOLS_CLEAR_CACHES),
        item("Tools", &TOOLS_OPEN_CODE_EDITOR),
        item("Tools", &TOOLS_BAKE_CAPTURES),
        item("Tools", &TOOLS_PROJECT_HEALTH),
        // Sequence ▸ Render movie… (ADR-7726): the dialog's Render is `editor.render_movie`.
        item_with(
            "Sequence",
            &SEQUENCE_RENDER_MOVIE,
            SEQUENCE_RENDER_MOVIE.label,
            json!({ "sequence": "<file.seq.ron>" }),
        ),
        item("Play", &PLAY_PLAY),
        item_with(
            "Play",
            &PLAY_SIMULATE,
            PLAY_SIMULATE.label,
            json!({ "mode": "simulate" }),
        ),
        item("Play", &PLAY_PAUSE),
        item("Play", &PLAY_RESUME),
        item("Play", &PLAY_STOP),
        item_with("Play", &PLAY_STEP, PLAY_STEP.label, json!({ "frames": 1 })),
        item("Play", &PLAY_COMPILE),
    ]);
    for players in 1..=MENU_PLAYERS {
        items.push(item_with(
            "Play",
            &PLAY_PLAYERS,
            &players_label(players),
            json!({ "players": players }),
        ));
    }
    items.extend([
        item_with(
            "Help",
            &HELP_DOCUMENTATION,
            HELP_DOCUMENTATION.label,
            json!({ "topic": "documentation" }),
        ),
        item_with(
            "Help",
            &HELP_AGENT_GUIDE,
            HELP_AGENT_GUIDE.label,
            json!({ "topic": "agent_guide" }),
        ),
        item_with(
            "Help",
            &HELP_SAMPLES,
            HELP_SAMPLES.label,
            json!({ "action": "list" }),
        ),
        item_with("Help", &HELP_ABOUT, HELP_ABOUT.label, json!({ "open": true })),
    ]);
    // One Window menu: Undo History (3255), Agent (3266), Notes and Activity (3259), the build tools (3261).
    items.extend([
        item("Window", &WINDOW_UNDO_HISTORY),
        item("Window", &WINDOW_AGENT),
        item("Window", &WINDOW_NOTES),
        item("Window", &WINDOW_ACTIVITY),
    ]);
    // Window ▸ New Outliner / Content Browser / Inspector (P-003, ADR-8105 (a)).
    // The shell draws these before the build-tool rows.
    items.extend([
        item_with(
            "Window",
            &WINDOW_NEW_OUTLINER,
            WINDOW_NEW_OUTLINER.label,
            json!({"panel":"outliner"}),
        ),
        item_with(
            "Window",
            &WINDOW_NEW_CONTENT,
            WINDOW_NEW_CONTENT.label,
            json!({"panel":"content"}),
        ),
        item_with(
            "Window",
            &WINDOW_NEW_INSPECTOR,
            WINDOW_NEW_INSPECTOR.label,
            json!({"panel":"inspector"}),
        ),
    ]);
    items.extend([
        item_with(
            "Window",
            &SIZE_REPORT_OPEN,
            SIZE_REPORT_OPEN.label,
            json!({"open":true}),
        ),
        item_with(
            "Window",
            &PLATFORMS_OPEN,
            PLATFORMS_OPEN.label,
            json!({"open":true}),
        ),
        item_with(
            "Window",
            &LAUNCH_PROFILES_OPEN,
            LAUNCH_PROFILES_OPEN.label,
            json!({"open":true}),
        ),
    ]);
    // Window ▸ Utility Panels (ADR-8189). The shell draws one row per scanned sidecar.
    // The static tree lists the starter, which every project shows. A project's extra
    // sidecars are drawn from the same scan when that project is open.
    for entry in crate::utility_panel::scan(None) {
        items.push(item_with(
            "Window",
            &WINDOW_UTILITY_PANEL,
            &format!("Utility Panels/{}", entry.title()),
            json!({"op":"open","id":entry.id}),
        ));
    }
    items
}

/// The Players submenu entry for `n` local players.
#[must_use]
pub fn players_label(n: u8) -> String {
    if n == 1 {
        "Players/1 player".to_string()
    } else {
        format!("Players/{n} players")
    }
}

/// Whether an item can run now (a disabled item draws greyed out).
#[must_use]
pub fn item_enabled(world: &World, item: &MenuItem) -> bool {
    let mode = world.get_resource::<crate::pie::PieStatus>().and_then(|s| s.mode);
    let selection = world
        .get_resource::<crate::selection::Selection>()
        .is_some_and(|s| !s.is_empty());
    let project = crate::mesh_source::project_root(world).is_some();
    let busy = crate::package_job::busy_reason(world).is_some();
    let undo = world.get_resource::<crate::command::UndoStack>();
    match item.action.id {
        "blockout.bake" => world
            .get_resource::<crate::selection::Selection>()
            .and_then(crate::selection::Selection::primary)
            .is_some_and(|e| world.get::<crate::blockout::BlockoutBrush>(e).is_some()),
        "edit.undo" => undo.is_some_and(|u| u.undo_depth() > 0),
        "edit.redo" => undo.is_some_and(|u| u.redo_depth() > 0),
        "edit.cut" | "edit.copy" | "edit.duplicate" | "edit.delete" => selection,
        "edit.paste" => world
            .get_resource::<crate::viewport_ops::EditorClipboard>()
            .is_some_and(|c| c.file.is_some()),
        "edit.paste_at_original" => world
            .get_resource::<crate::viewport_ops::EditorClipboard>()
            .is_some_and(|c| c.file.is_some()),
        "edit.paste_as_child" => {
            selection
                && world
                    .get_resource::<crate::viewport_ops::EditorClipboard>()
                    .is_some_and(|c| c.file.is_some())
        }
        "file.recent" => world
            .get_resource::<crate::viewport_ops::RecentLevels>()
            .is_some_and(|r| !r.paths.is_empty()),
        "file.package" | "tools.clear_caches" | "tools.rebuild_asset_db" => project && !busy,
        "file.version_history" | "file.duplicate_project" => project,
        "tools.open_code_editor" => project,
        "view.layout_save" => project,
        "play.compile" => compile_enabled(world),
        "play.play" | "play.simulate" | "play.players" => mode.is_none(),
        "play.pause" => mode == Some(crate::pie::PlayMode::Playing),
        "play.resume" | "play.step" => mode == Some(crate::pie::PlayMode::Paused),
        "play.stop" => mode.is_some(),
        "tools.bake_captures" => {
            project
                && !world
                    .get_resource::<crate::capture_bake::CaptureBakeJob>()
                    .is_some_and(crate::capture_bake::CaptureBakeJob::running)
        }
        "sequence.render_movie" => project,
        _ => true,
    }
}

// ---------------------------------------------------------------------------------------------
// Tools ▸ Rebuild asset database

/// What [`rebuild_asset_db`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RebuildReport {
    /// Sources found under `assets/`.
    pub sources: usize,
    /// Database rows kept (their source still exists).
    pub kept: usize,
    /// Rows dropped because their source is gone (project-relative paths).
    pub dropped: Vec<String>,
    /// `.meta` sidecars moved next to their moved source.
    pub metas_repaired: usize,
    /// `.meta` sidecars still orphaned.
    pub metas_orphaned: usize,
}

impl RebuildReport {
    /// The protocol object.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "sources": self.sources,
            "kept": self.kept,
            "dropped": self.dropped,
            "metas_repaired": self.metas_repaired,
            "metas_orphaned": self.metas_orphaned,
        })
    }
}

/// Whether Play ▸ Compile can run: the project has game code and no build runs (the toolbar's
/// rule).
#[must_use]
pub fn compile_enabled(world: &World) -> bool {
    let code = world.get_resource::<crate::code_reload::CodeReloadStatus>();
    code.is_some_and(|code| {
        code.game.is_some() && !matches!(code.state, Some(crate::hot_reload::CompileState::Compiling(_)))
    })
}

/// Takes the project's cook lock (`<state>/cook.lock`, the asset pipeline's) for the database
/// rewrite, so a live cook never saves over it from a stale snapshot. Never waits: a lock another
/// process holds is `tools_busy` (the caller is the UI thread). Best effort like the pipeline's:
/// a volume without file locks rebuilds unlocked (`Ok(None)`).
fn cook_lock(root: &Path) -> Result<Option<std::fs::File>, (&'static str, String)> {
    let dir = regia_core::brand::project_state_dir(root);
    if std::fs::create_dir_all(&dir).is_err() {
        return Ok(None);
    }
    let path = dir.join("cook.lock");
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
    else {
        return Ok(None);
    };
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Err((
            "tools_busy",
            format!(
                "a cook holds {}: rebuild the asset database once it ends",
                path.display()
            ),
        )),
        Err(std::fs::TryLockError::Error(_)) => Ok(None),
    }
}

/// Tools ▸ Rebuild asset database for the project at `root`: re-pairs moved `.meta` sidecars,
/// drops rows whose source no longer exists and saves the database. The next cook re-imports
/// new and changed sources (it compares hashes).
///
/// # Errors
/// `tools_no_project` (no `Regia.toml`), `tools_busy` (another process holds the cook lock) or
/// `tools_failed` (the database cannot be read or written).
pub fn rebuild_asset_db(root: &Path) -> Result<RebuildReport, (&'static str, String)> {
    let _span = tracing::info_span!(target: "regia_editor", "rebuild_asset_db").entered();
    if !regia_core::brand::is_project_root(root) {
        return Err((
            "tools_no_project",
            format!("{} has no Regia.toml: open a project first", root.display()),
        ));
    }
    let _lock = cook_lock(root)?;
    let mut db =
        regia_asset_pipeline::AssetDatabase::open(root).map_err(|e| ("tools_failed", e.to_string()))?;
    let repair = db.repair_orphaned_metas(root, true);
    let sources: BTreeSet<String> =
        regia_asset_pipeline::AssetDatabase::discover_sources(&root.join("assets"))
            .iter()
            .map(|source| {
                source
                    .strip_prefix(root)
                    .unwrap_or(source)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
    let dropped: Vec<String> = db
        .entries
        .keys()
        .filter(|key| !sources.contains(key.as_str()))
        .cloned()
        .collect();
    db.entries.retain(|key, _| sources.contains(key.as_str()));
    db.save(root).map_err(|e| ("tools_failed", e.to_string()))?;
    Ok(RebuildReport {
        sources: sources.len(),
        kept: db.entries.len(),
        dropped,
        metas_repaired: repair.repaired.len(),
        metas_orphaned: repair.orphans.len(),
    })
}

/// [`rebuild_asset_db`] for the editor's project, then re-lists the Content Browser index.
///
/// # Errors
/// As [`rebuild_asset_db`]; `tools_no_project` without a project, `tools_busy` while a package,
/// Clear caches or agent cook, package or clean job runs.
pub fn rebuild_for_world(world: &mut World) -> Result<RebuildReport, (&'static str, String)> {
    let root = crate::mesh_source::project_root(world)
        .ok_or_else(|| ("tools_no_project", "open a project first".to_string()))?;
    if let Some(running) = crate::package_job::busy_reason(world) {
        return Err(("tools_busy", running));
    }
    let report = rebuild_asset_db(&root)?;
    crate::content_index::invalidate(world);
    Ok(report)
}

fn tools_error(reason: &str, message: &str) -> RpcError {
    let (code, regia) = if reason == "tools_no_project" {
        (NOT_FOUND, "F0003")
    } else {
        (INVALID_PARAMS, "F0002")
    };
    let mut error = RpcError::new(
        code,
        regia,
        message.to_string(),
        "see book/agents/editor-menus.md",
    );
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), Value::String(reason.to_string()));
    }
    error
}

// ---------------------------------------------------------------------------------------------
// Tools ▸ Open code editor (spec 19 § 5)

/// Tools ▸ Open code editor (spec 19 § 5): runs the project's configured code editor — the
/// `code.editor_command` preference, else `[editor] code_editor` of `.regia/agent.toml`, else the
/// environment's `$VISUAL`/`$EDITOR` through `env` — on the game crate's manifest (`Regia.toml`
/// when the project has no game crate), spawned with the project root as the working directory.
/// The same resolution a log link's click uses (`crate::log_links`).
///
/// # Errors
///
/// `open_in_editor_no_code_editor` when nothing names an editor (`data.reason`), or the program
/// fails to start.
pub fn open_code_editor(world: &mut World, env: &dyn Fn(&str) -> Option<String>) -> Result<Value, RpcError> {
    open_code_editor_child(world, env).map(|(reply, _editor)| reply)
}

/// [`open_code_editor`] that also hands back the started editor process (the tests wait on it
/// before reading what it wrote).
fn open_code_editor_child(
    world: &mut World,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(Value, std::process::Child), RpcError> {
    let root = regia_agent::req_path::project_root(world);
    let game = root.join("game").join("Cargo.toml");
    let file = if game.is_file() {
        game
    } else {
        root.join("Regia.toml")
    };
    let command = crate::log_links::click_command(world, &root, &file, 1, None, env)?;
    let editor = crate::agent_changes::spawn_code_editor_child(&root, &command)?;
    let reply = json!({
        "opened": "code_editor",
        "root": root.display().to_string(),
        "file": file.display().to_string(),
        "command": command,
    });
    Ok((reply, editor))
}

/// The `editor.open_code_editor` handler: the real environment's `$VISUAL`/`$EDITOR` are the last
/// fallback.
fn open_code_editor_method(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let env = |name: &str| regia_core::brand::env_var_os(name).map(|v| v.to_string_lossy().into_owned());
    open_code_editor(world, &env)
}

// ---------------------------------------------------------------------------------------------
// Drawing: Tools, Play and Help (File, Create, Edit and View stay in the shell)

/// The rich tooltip of a menu action (ADR-7767): its description and agent route, with the
/// shortcut of the palette command it runs.
fn rich_tip(action: &parity::EditorAction, description: &str) -> crate::tooltips::RichTooltip {
    crate::tooltips::RichTooltip::for_action(action, description, None)
}

/// The Clear caches confirm window's state.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct ClearCachesConfirm {
    /// Whether the window is open.
    pub open: bool,
}

/// The Tools menu. Answers a toast line when an item ran.
pub fn tools_menu(ui: &mut egui::Ui, world: &mut World) -> Option<String> {
    let mut toast = None;
    let project = crate::mesh_source::project_root(world).is_some();
    let busy = crate::package_job::busy_reason(world).is_some();
    if parity::action_enabled(ui, project && !busy, &parity::TOOLS_REBUILD_ASSET_DB, None)
        .on_hover_ui(|ui| {
            rich_tip(
                &parity::TOOLS_REBUILD_ASSET_DB,
                "Re-pair moved .meta files, drop rows of deleted sources, re-list the Content Browser",
            )
            .show(ui);
        })
        .clicked()
    {
        toast = Some(match rebuild_for_world(world) {
            Ok(report) => format!(
                "Asset database rebuilt: {} sources, {} stale rows dropped, {} .meta repaired",
                report.sources,
                report.dropped.len(),
                report.metas_repaired
            ),
            Err((reason, message)) => format!("{reason}: {message}"),
        });
        ui.close();
    }
    if parity::action_enabled(ui, project && !busy, &parity::TOOLS_CLEAR_CACHES, None)
        .on_hover_ui(|ui| {
            rich_tip(
                &parity::TOOLS_CLEAR_CACHES,
                "Delete the cook, asset-database, shader and pipeline caches (asks first)",
            )
            .show(ui);
        })
        .clicked()
    {
        world.insert_resource(ClearCachesConfirm { open: true });
        ui.close();
    }
    // Open code editor (spec 19 § 5): the configured editor opens the game crate from the root.
    if parity::action_enabled(ui, project, &parity::TOOLS_OPEN_CODE_EDITOR, None)
        .on_hover_ui(|ui| {
            rich_tip(
                &parity::TOOLS_OPEN_CODE_EDITOR,
                "Open the game crate in the configured code editor (the code.editor_command \
                 preference, .regia/agent.toml, or $VISUAL/$EDITOR)",
            )
            .show(ui);
        })
        .clicked()
    {
        let env = |name: &str| regia_core::brand::env_var_os(name).map(|v| v.to_string_lossy().into_owned());
        toast = Some(match open_code_editor(world, &env) {
            Ok(out) => format!(
                "Opened the code editor ({})",
                out["file"].as_str().unwrap_or_default()
            ),
            Err(error) => error.message,
        });
        ui.close();
    }
    // Tools ▸ Bake reflection captures (a cancellable job, ADR-7592).
    if let Some(message) = crate::capture_bake::menu_item(ui, world) {
        toast = Some(message);
    }
    // Tools ▸ Project Health (ADR-7789).
    if let Some(message) = crate::project_health::menu_item(ui, world) {
        toast = Some(message);
        ui.close();
    }
    toast
}

/// The Play menu: the toolbar's Play, Simulate, Pause, Resume, Stop, Step, Compile, and the
/// Players submenu, each greyed out when it cannot run (the items of `editor.menu_list`). Answers
/// the request to queue; Compile inserts the toolbar's `CompileRequest` itself.
pub fn play_menu(ui: &mut egui::Ui, world: &mut World) -> Option<crate::pie::PieRequest> {
    use crate::pie::{PieRequest, PlayKind, PlayMode, PlayOptions};
    let mode = world.get_resource::<crate::pie::PieStatus>().and_then(|s| s.mode);
    let stopped = mode.is_none();
    let mut request = None;
    if parity::action_enabled(ui, stopped, &parity::PLAY_PLAY, None).clicked() {
        request = Some(PieRequest::Play);
    }
    if parity::action_enabled(ui, stopped, &parity::PLAY_SIMULATE, None).clicked() {
        request = Some(PieRequest::PlayWith(PlayOptions {
            kind: PlayKind::Simulate,
            ..PlayOptions::default()
        }));
    }
    if parity::action_enabled(ui, mode == Some(PlayMode::Playing), &parity::PLAY_PAUSE, None).clicked() {
        request = Some(PieRequest::Pause);
    }
    if parity::action_enabled(ui, mode == Some(PlayMode::Paused), &parity::PLAY_RESUME, None).clicked() {
        request = Some(PieRequest::Resume);
    }
    if parity::action_enabled(ui, !stopped, &parity::PLAY_STOP, None).clicked() {
        request = Some(PieRequest::Stop);
    }
    if parity::action_enabled(ui, mode == Some(PlayMode::Paused), &parity::PLAY_STEP, None).clicked() {
        request = Some(PieRequest::Step(1));
    }
    // The toolbar's Compile (specs/19 § 5): rebuild and hot-reload the game code.
    if parity::action_enabled(ui, compile_enabled(world), &parity::PLAY_COMPILE, None)
        .on_hover_ui(|ui| {
            let mut tip = rich_tip(&parity::PLAY_COMPILE, "Rebuild and hot-reload the game code");
            tip.shortcut = Some("Ctrl+Alt+F11".to_owned());
            tip.show(ui);
        })
        .clicked()
    {
        world.insert_resource(crate::code_reload::CompileRequest(true));
        ui.close();
    }
    ui.add_enabled_ui(stopped, |ui| {
        parity::menu(ui, parity::PLAY_PLAYERS.label, |ui| {
            for players in 1..=MENU_PLAYERS {
                let label = players_label(players);
                let text = label.trim_start_matches("Players/").to_string();
                if parity::action_button_text(ui, &parity::PLAY_PLAYERS, text).clicked() {
                    request = Some(PieRequest::PlayWith(PlayOptions {
                        players,
                        ..PlayOptions::default()
                    }));
                    ui.close();
                }
            }
        });
    });
    if request.is_some() {
        ui.close();
    }
    request
}

/// The Sequence menu (ADR-7726): Render movie… opens the dialog (`crate::render_out_job`). Answers
/// a toast line.
pub fn sequence_menu(ui: &mut egui::Ui, world: &mut World) -> Option<String> {
    crate::render_out_job::menu_item(ui, world)
}

/// The Help menu. Answers a toast line.
pub fn help_menu(ui: &mut egui::Ui, world: &mut World) -> Option<String> {
    let mut toast = None;
    for (action, topic) in [
        (
            &parity::HELP_DOCUMENTATION,
            crate::about::HelpTopic::Documentation,
        ),
        (&parity::HELP_AGENT_GUIDE, crate::about::HelpTopic::AgentGuide),
    ] {
        let what = match topic {
            crate::about::HelpTopic::Documentation => "Open the local book in the OS viewer",
            crate::about::HelpTopic::AgentGuide => "Open the agent protocol pages of the local book",
        };
        if parity::action_menu_item(ui, action)
            .on_hover_ui(|ui| rich_tip(action, what).show(ui))
            .clicked()
        {
            toast = Some(match crate::about::open_help(topic, false) {
                Ok(path) => format!("Opened {}", path.display()),
                Err(error) => format!("{}: {}", error.reason, error.message),
            });
            ui.close();
        }
    }
    ui.separator();
    if parity::action_menu_item(ui, &parity::HELP_SAMPLES)
        .on_hover_ui(|ui| {
            rich_tip(
                &parity::HELP_SAMPLES,
                "Copy a template, sample or extension example into a new folder",
            )
            .show(ui);
        })
        .clicked()
    {
        crate::samples_browser::open_window(world, None);
        ui.close();
    }
    if parity::action_menu_item(ui, &parity::HELP_ABOUT)
        .on_hover_ui(|ui| {
            let mut tip = rich_tip(&parity::HELP_ABOUT, "Version, git revision and license");
            tip.description
                .push_str(". F1 (help.context) opens the help of the focused panel.");
            tip.show(ui);
        })
        .clicked()
    {
        crate::about::open_about(world);
        ui.close();
    }
    toast
}

/// Draws the Clear caches confirm window, the package dialog and the About window. The shell
/// calls it every frame. Answers a toast line.
pub fn windows(ctx: &egui::Context, world: &mut World) -> Option<String> {
    crate::package_job::show(ctx, world);
    // File ▸ Screenshot…'s dialog and its toast (ADR-7768).
    crate::screenshot::window(ctx, world);
    crate::platforms_panel::show(ctx, world);
    crate::launch_profiles::show(ctx, world);
    crate::size_report_panel::show(ctx, world);
    crate::about::show(ctx, world);
    // Help ▸ Samples… (w13).
    crate::samples_browser::window(ctx, world);
    // The reflection capture bake's progress window and Cancel (ADR-7592).
    crate::capture_bake::window(ctx, world);
    // Sequence ▸ Render movie…: the dialog, its progress and Cancel (ADR-7726).
    crate::render_out_job::window(ctx, world);
    let confirm = world
        .get_resource::<ClearCachesConfirm>()
        .copied()
        .unwrap_or_default();
    if !confirm.open {
        return None;
    }
    let mut toast = None;
    let mut close = false;
    egui::Window::new("Clear caches?")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.label(
                "Delete the cook cache, the asset database and the shader and pipeline caches of this \
                 project? Sources and assets/ are kept; the next cook and launch rebuild the caches.",
            );
            ui.horizontal(|ui| {
                if parity::action_button(ui, &parity::TOOLS_CLEAR_CACHES_CONFIRM).clicked() {
                    toast = Some(match crate::package_job::start_clear_caches(world) {
                        Ok(job) => format!("Clearing caches ({})", job.id),
                        Err(error) => error.to_string(),
                    });
                    close = true;
                }
                if parity::action_button(ui, &parity::TOOLS_CLEAR_CACHES_CANCEL).clicked() {
                    close = true;
                }
            });
        });
    if close {
        world.insert_resource(ClearCachesConfirm { open: false });
    }
    toast
}

// ---------------------------------------------------------------------------------------------
// Agent methods

/// `editor.menu_list`.
fn menu_list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let items = menu_tree();
    let menus: Vec<Value> = MENUS
        .iter()
        .map(|menu| {
            let rows: Vec<Value> = items
                .iter()
                .filter(|item| item.menu == *menu)
                .map(|item| item.to_json(item_enabled(world, item)))
                .collect();
            json!({ "name": menu, "items": rows })
        })
        .collect();
    Ok(json!({ "menus": menus, "count": items.len() }))
}

/// `editor.rebuild_asset_db`.
fn rebuild_asset_db_method(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    rebuild_for_world(world)
        .map(|report| report.to_json())
        .map_err(|(reason, message)| tools_error(reason, &message))
}

/// The methods of this module.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.menu_list",
                tool: "menu_list",
                description: "The editor's whole menu tree (File, Create, Edit, View, Window, Tools, Play, Help): every \
                    item's path, label, registered action id and class, the agent method and the parameters \
                    that do what clicking it does, and whether it is enabled now.",
                params: &[],
                result: "{ menus: [{ name, items: [{ path, label, action, class, method, params, enabled }] }], count }",
                mutating: false,
                permission: Permission::Read,
            },
            menu_list as Handler,
        ),
        (
            MethodSchema {
                name: "editor.rebuild_asset_db",
                tool: "rebuild_asset_db",
                description: "Tools ▸ Rebuild asset database: re-pair moved .meta sidecars, drop database rows \
                    whose source is gone, save the database (under the cook lock) and re-list the Content \
                    Browser; the next cook re-imports changed sources. tools_no_project without a project, \
                    tools_busy while a package, Clear caches or cook runs or another process holds the \
                    cook lock (never waits), tools_failed when the database cannot be read or written.",
                params: &[],
                result: "{ sources, kept, dropped, metas_repaired, metas_orphaned }",
                mutating: true,
                permission: Permission::Assets,
            },
            rebuild_asset_db_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.open_code_editor",
                tool: "open_code_editor",
                description: "Tools ▸ Open code editor: run the project's configured code editor (the \
                    code.editor_command preference, else [editor] code_editor of .regia/agent.toml, else \
                    $VISUAL/$EDITOR) on the game crate's manifest from the project root. \
                    open_in_editor_no_code_editor when nothing names an editor. Starts an external process: \
                    needs the build permission and spends the mutating-call budget.",
                params: &[],
                result: "{ opened, root, file, command }",
                mutating: true,
                permission: Permission::Build,
            },
            open_code_editor_method as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_item_has_an_agent_method_and_a_unique_path() {
        let items = menu_tree();
        let mut paths = BTreeSet::new();
        for item in &items {
            assert!(!item.method.is_empty(), "{} has no agent method", item.path());
            assert!(MENUS.contains(&item.menu), "{}", item.path());
            assert!(paths.insert(item.path()), "duplicate menu path {}", item.path());
        }
        for menu in ["Tools", "Play", "Help"] {
            assert!(items.iter().any(|i| i.menu == menu), "{menu} has items");
        }
    }

    #[test]
    fn every_shell_menu_action_is_in_the_tree() {
        const PLAY_TOOLBAR_ONLY: &[&str] = &[
            "play.start_player_start",
            "play.start_camera",
            "play.start_entity",
            "play.mute",
            "play.view_server",
            "play.view_client",
            "play.device",
            "play.device_rotate",
            "play.device_touch",
            "play.device_gamepad",
            // Safe-area outline is a play-toolbar toggle (ADR-8205), same as the other device controls.
            "play.device_safe_area",
        ];
        let items = menu_tree();
        for action in parity::ACTIONS.iter().filter(|a| {
            matches!(
                a.panel,
                "shell.file"
                    | "shell.create"
                    | "shell.edit"
                    | "shell.view"
                    | "shell.window"
                    | "shell.tools"
                    | "shell.play"
                    | "shell.help"
            )
        }) {
            // The confirm window's buttons and the About box's file links are not menu items.
            if action.id.starts_with("tools.clear_caches_") || action.id.starts_with("help.about_") {
                continue;
            }
            // The play toolbar's start menu, mute toggle, viewing switch and device controls
            // (ADR-7829) are toolbar controls, not menu items; agent_parity_shell checks them.
            if PLAY_TOOLBAR_ONLY.contains(&action.id) {
                continue;
            }
            assert!(
                items.iter().any(|item| item.action.id == action.id),
                "the menu tree misses {} ({})",
                action.id,
                action.label
            );
        }
    }

    #[test]
    fn rebuild_drops_rows_of_deleted_sources_and_keeps_the_rest() {
        let root = regia_test_utils::unique_temp_dir("menus_rebuild_db");
        std::fs::create_dir_all(root.join("assets/props")).expect("assets");
        std::fs::write(root.join("Regia.toml"), "[project]\nname = \"t\"\n").expect("manifest");
        std::fs::write(root.join("assets/props/rock.png"), b"png").expect("rock");
        let mut db = regia_asset_pipeline::AssetDatabase::default();
        for key in ["assets/props/rock.png", "assets/props/gone.png"] {
            db.entries.insert(
                key.to_string(),
                regia_asset_pipeline::DbEntry {
                    source_path: key.to_string(),
                    asset_id: regia_asset::AssetId::new_random(),
                    source_hash: [1; 32],
                    settings_hash: [2; 32],
                    importer: "texture".into(),
                    importer_version: 1,
                    cache_key: [3; 32],
                    cooked_rel: "x".into(),
                },
            );
        }
        db.save(&root).expect("save");
        let report = rebuild_asset_db(&root).expect("rebuild");
        assert_eq!(report.dropped, ["assets/props/gone.png"]);
        assert_eq!((report.sources, report.kept), (1, 1));
        let after = regia_asset_pipeline::AssetDatabase::open(&root).expect("open");
        let kept: Vec<&String> = after.entries.keys().collect();
        assert_eq!(kept, ["assets/props/rock.png"]);
        assert_eq!(
            after.entries["assets/props/rock.png"].source_hash, [1; 32],
            "kept rows are untouched"
        );

        let not_project = regia_test_utils::unique_temp_dir("menus_rebuild_none");
        std::fs::create_dir_all(&not_project).expect("dir");
        assert_eq!(
            rebuild_asset_db(&not_project).map_err(|e| e.0),
            Err("tools_no_project")
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&not_project).ok();
    }

    /// A project with a game crate and a fake code editor script on it (a `/bin/sh` script, so
    /// unix only).
    #[cfg(unix)]
    fn code_editor_project(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let root = regia_test_utils::unique_temp_dir(tag);
        std::fs::create_dir_all(root.join("game/src")).expect("game");
        std::fs::write(root.join("Regia.toml"), "[project]\nname = \"t\"\n").expect("manifest");
        std::fs::write(root.join("game/Cargo.toml"), "[package]\nname = \"game\"\n").expect("game manifest");
        let bin = regia_test_utils::unique_temp_dir(&format!("{tag}_bin"));
        let script = bin.join("regia-fake-code-editor");
        let log = bin.join("ran.log");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\npwd > \"{log}\"\nfor a in \"$@\"; do echo \"$a\" >> \"{log}\"; done\n",
                log = log.display()
            ),
        )
        .expect("script");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).expect("chmod");
        }
        (root, script, log)
    }

    fn world_at(root: &Path) -> World {
        let mut world = World::new();
        world.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.to_path_buf()),
        });
        world
    }

    fn reason(error: &RpcError) -> String {
        error
            .data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    /// Tools ▸ Open code editor (spec 19 § 5): the configured editor command runs from the
    /// project root on the game crate, and nothing configured answers the documented code. The
    /// test waits for the spawned fake editor to exit before reading its log (no timing race).
    #[cfg(unix)]
    #[test]
    fn open_code_editor_runs_the_configured_editor_at_the_project_root() {
        let (root, script, log) = code_editor_project("menus_code_editor");
        let mut world = world_at(&root);
        let mut prefs = crate::editor_prefs::EditorPreferences::default();
        prefs
            .set(
                crate::log_links::CODE_EDITOR_PREF,
                &crate::editor_prefs::PrefValue::Text(script.display().to_string()),
            )
            .expect("pref");
        world.insert_resource(prefs);
        let (out, mut editor) = open_code_editor_child(&mut world, &|_| None).expect("the editor ran");
        let status = editor.wait().expect("wait for the fake editor");
        assert!(status.success(), "the fake editor exited cleanly: {status}");
        assert_eq!(out["opened"], "code_editor", "{out}");
        assert_eq!(
            out["command"][0],
            script.display().to_string(),
            "the configured command, not a fallback"
        );
        let ran = std::fs::read_to_string(&log).expect("the fake editor was run");
        let mut lines = ran.lines();
        assert_eq!(
            lines.next().unwrap_or_default(),
            root.display().to_string(),
            "spawned with the project root as the working directory"
        );
        assert!(
            ran.lines()
                .any(|a| a.replace('\\', "/").ends_with("game/Cargo.toml")),
            "the game crate opens: {ran}"
        );
        // Nothing configured: the documented refusal, not a crash.
        let err = open_code_editor(&mut world_at(&root), &|_| None).expect_err("no editor");
        assert_eq!(reason(&err), "open_in_editor_no_code_editor", "{err:?}");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(script.parent().unwrap_or(Path::new(".")));
    }

    /// w10-editor-agent-smalls (3): the method starts an external program, so it is declared like
    /// the other process-spawning methods (`editor.launch`, `editor.package_start`): mutating, with the
    /// `build` permission, never `Read`.
    #[test]
    fn open_code_editor_is_declared_mutating_with_the_build_permission() {
        let (schema, _) = methods()
            .into_iter()
            .find(|(m, _)| m.name == "editor.open_code_editor")
            .expect("registered");
        assert!(schema.mutating, "it spawns a process");
        assert_eq!(schema.permission, Permission::Build);
    }

    /// Tools ▸ Open code editor with nothing configured (no preference, no `.regia/agent.toml`,
    /// no `$VISUAL`/`$EDITOR`) answers the documented refusal on every platform.
    #[test]
    fn open_code_editor_without_an_editor_is_refused() {
        let root = regia_test_utils::unique_temp_dir("menus_code_editor_none");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::write(root.join("Regia.toml"), "[project]\nname = \"t\"\n").expect("manifest");
        let err = open_code_editor(&mut world_at(&root), &|_| None).expect_err("no editor");
        assert_eq!(reason(&err), "open_in_editor_no_code_editor", "{err:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
