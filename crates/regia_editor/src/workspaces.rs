// SPDX-License-Identifier: MPL-2.0
//! Named layouts, workspaces, maximize and the full-screen viewport, and pane management
//! (`docs/specs/19-editor.md` § 2, the v0.6 parity addendum § 1.1-1.4, ADR-7753).
//!
//! - **Named layouts.** The dock arrangement saved under a name, one file per layout in the user
//!   config directory: `<config>/layouts/<name>.editor_layout.ron`, in the `editor_layout.ron` shape
//!   (version 2, [`crate::editor_layout::LayoutDoc`]), so no new format. A layout saved as a
//!   *workspace* is `<name>.workspace.editor_layout.ron`: the flag is the file name, the file
//!   itself is the same shape. Names are 1-64 ASCII letters, digits, spaces, `-` and `_` (so the
//!   suffix can never be part of a name), compared without case.
//! - **Workspaces.** A workspace is a layout, its panels and an optional tool mode: the built-in
//!   [`BUILT_IN_WORKSPACES`] (including Terrain and 2D tiles), live extension workspaces and the user's
//!   workspace layouts (`user:<name>`). Switching parks the shown dock under the workspace it
//!   leaves and shows the target's parked dock, else its built-in or saved layout, so going back
//!   restores the exact tree. Documents live in their editors' state, not in the tabs, so a switch
//!   never closes one.
//! - **Maximize.** `view.maximize_panel` (Ctrl+Space) shows one tab alone in the whole dock area and
//!   keeps the previous dock; the same command puts it back unchanged. `view.fullscreen_viewport`
//!   (F11) does the same for the Viewport and also hides the menu bar, toolbars and status bar.
//!   Escape leaves full screen, but while a play session runs Escape stops play first
//!   ([`escape_action_full`]).
//! - **Pane management.** Each tab group has a `+` button whose popup lists the group's tabs (pick one
//!   to focus it, which is also the overflow list of a crowded tab bar) and every panel to add; a
//!   tab's context menu has Replace with, Maximize, Join with neighbor and Swap with. Dragging an item over a tab for
//!   [`DWELL_FRAMES`] frames brings that tab forward.
//!
//! Agent surface: `editor.layout_list`, `editor.layout_save_named`, `editor.layout_apply_named`,
//! `editor.layout_delete`, `editor.workspace_list`, `editor.workspace_set`, `editor.maximize_panel`
//! and `editor.panel_replace` (`book/agents/editor-layouts.md`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use egui_dock::{DockState, NodeIndex};
use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::editor_layout::{LayoutDoc, LayoutError, LayoutRequest, LayoutRequests, LayoutSnapshot};
use crate::shell::Panel;

/// The folder of named layouts, inside the user config directory.
pub const NAMED_LAYOUT_DIR: &str = "layouts";
/// The file suffix of a named layout.
pub const LAYOUT_FILE_SUFFIX: &str = ".editor_layout.ron";
/// The file suffix of a named layout flagged as a workspace.
pub const WORKSPACE_FILE_SUFFIX: &str = ".workspace.editor_layout.ron";
/// The longest layout name, in characters.
pub const MAX_NAME_LEN: usize = 64;
/// The most named layouts the folder may hold.
pub const MAX_NAMED_LAYOUTS: usize = 256;
/// The id prefix of a user workspace (`user:<layout name>`).
pub const USER_WORKSPACE_PREFIX: &str = "user:";
/// The id prefix of the palette commands that switch workspace (`workspace.<id>`).
pub const WORKSPACE_COMMAND_PREFIX: &str = "workspace.";
/// The maximize command id (built-in shortcut Ctrl+Space, rebindable).
pub const MAXIMIZE_COMMAND: &str = "view.maximize_panel";
/// The full-screen viewport command id (built-in shortcut F11, rebindable).
pub const FULLSCREEN_COMMAND: &str = "view.fullscreen_viewport";
/// Frames an item must be dragged over a tab before the tab comes forward (frames, not wall
/// time, so a slow frame never switches early).
pub const DWELL_FRAMES: u64 = 30;
/// The workspace a session starts in.
pub const LEVEL_WORKSPACE: &str = "level";

// ---------------------------------------------------------------------------------------------
// Errors

/// Why a layout, workspace or pane request was refused. `code` is the agent error code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceError {
    /// Lowercase agent error code (listed in `book/agents/editor-layouts.md`).
    pub code: &'static str,
    /// What went wrong, for people.
    pub message: String,
}

impl WorkspaceError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The agent error.
    #[must_use]
    pub fn to_rpc(&self) -> RpcError {
        RpcError::new(INVALID_PARAMS, self.code, self.message.clone(), "")
    }
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WorkspaceError {}

impl From<LayoutError> for WorkspaceError {
    fn from(err: LayoutError) -> Self {
        Self::new(err.code(), err.to_string())
    }
}

// ---------------------------------------------------------------------------------------------
// Named layouts

/// One named layout file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedLayout {
    /// Its name.
    pub name: String,
    /// Whether it is a user workspace.
    pub workspace: bool,
    /// The file.
    pub path: PathBuf,
}

/// Checks and trims a layout name: 1-[`MAX_NAME_LEN`] ASCII letters, digits, spaces, `-` and `_`.
///
/// # Errors
///
/// `bad_layout_name`.
pub fn check_layout_name(name: &str) -> Result<String, WorkspaceError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME_LEN {
        return Err(WorkspaceError::new(
            "bad_layout_name",
            format!("a layout name has 1 to {MAX_NAME_LEN} characters"),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_'))
    {
        return Err(WorkspaceError::new(
            "bad_layout_name",
            format!("{name:?}: a layout name uses letters, digits, spaces, - and _ only"),
        ));
    }
    Ok(name.to_owned())
}

/// The named layout folder inside the user config directory `config_dir`.
#[must_use]
pub fn named_layout_dir(config_dir: &Path) -> PathBuf {
    config_dir.join(NAMED_LAYOUT_DIR)
}

/// The file of layout `name` (a checked name) in `dir`.
#[must_use]
pub fn named_layout_path(dir: &Path, name: &str, workspace: bool) -> PathBuf {
    let suffix = if workspace {
        WORKSPACE_FILE_SUFFIX
    } else {
        LAYOUT_FILE_SUFFIX
    };
    dir.join(format!("{name}{suffix}"))
}

/// The name and workspace flag a file name stands for (`None` for other files).
fn parse_file_name(file: &str) -> Option<(String, bool)> {
    let (stem, workspace) = match file.strip_suffix(WORKSPACE_FILE_SUFFIX) {
        Some(stem) => (stem, true),
        None => (file.strip_suffix(LAYOUT_FILE_SUFFIX)?, false),
    };
    // Only names this module could have written (a hand-made `a.b.editor_layout.ron` is ignored).
    check_layout_name(stem)
        .ok()
        .filter(|name| name == stem)
        .map(|name| (name, workspace))
}

/// Every named layout in `dir`, by name without case (a missing folder lists none). When a name
/// has both a plain and a workspace file, the workspace one wins.
#[must_use]
pub fn list_named_layouts(dir: &Path) -> Vec<NamedLayout> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<NamedLayout> = Vec::new();
    for entry in entries.flatten() {
        let file = entry.file_name();
        let Some((name, workspace)) = file.to_str().and_then(parse_file_name) else {
            continue;
        };
        match out.iter().position(|l| l.name.eq_ignore_ascii_case(&name)) {
            Some(i) => {
                if workspace && !out[i].workspace {
                    out[i] = NamedLayout {
                        name,
                        workspace,
                        path: entry.path(),
                    };
                }
            }
            None => out.push(NamedLayout {
                name,
                workspace,
                path: entry.path(),
            }),
        }
    }
    out.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then(a.name.cmp(&b.name))
    });
    out
}

/// The layout called `name` (without case), if saved.
#[must_use]
pub fn find_named_layout(dir: &Path, name: &str) -> Option<NamedLayout> {
    let name = name.trim();
    let all = list_named_layouts(dir);
    all.iter()
        .find(|l| l.name == name)
        .or_else(|| all.iter().find(|l| l.name.eq_ignore_ascii_case(name)))
        .cloned()
}

/// Saves `doc` as layout `name` (replacing a layout of that name, with either flag).
///
/// # Errors
///
/// `bad_layout_name`, `too_many_layouts`, `layout_newer` (a newer editor's file is in the way) or
/// `layout_io`.
pub fn save_named_layout(
    dir: &Path,
    name: &str,
    doc: &LayoutDoc,
    workspace: bool,
) -> Result<NamedLayout, WorkspaceError> {
    let name = check_layout_name(name)?;
    let same = files_named(dir, &name);
    if same.is_empty() && list_named_layouts(dir).len() >= MAX_NAMED_LAYOUTS {
        return Err(WorkspaceError::new(
            "too_many_layouts",
            format!("{MAX_NAMED_LAYOUTS} named layouts are saved; delete one first"),
        ));
    }
    std::fs::create_dir_all(dir)
        .map_err(|e| WorkspaceError::new("layout_io", format!("cannot create {}: {e}", dir.display())))?;
    let path = named_layout_path(dir, &name, workspace);
    // Another spelling with the same flag is renamed onto the new one first, so a case-insensitive
    // file system never ends with the save deleted below.
    for old in same
        .iter()
        .filter(|old| old.workspace == workspace && old.path != path)
    {
        std::fs::rename(&old.path, &path).map_err(|e| {
            WorkspaceError::new("layout_io", format!("cannot rename {}: {e}", old.path.display()))
        })?;
    }
    crate::editor_layout::write_layout_file(&path, doc)?;
    // One file per name: the other flag's file goes.
    for old in same.iter().filter(|old| old.workspace != workspace) {
        remove_file(&old.path)?;
    }
    Ok(NamedLayout {
        name,
        workspace,
        path,
    })
}

/// Every layout file in `dir` whose name is `name` without case (both flags, every spelling).
fn files_named(dir: &Path, name: &str) -> Vec<NamedLayout> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let (found, workspace) = entry.file_name().to_str().and_then(parse_file_name)?;
            found.eq_ignore_ascii_case(name).then(|| NamedLayout {
                name: found,
                workspace,
                path: entry.path(),
            })
        })
        .collect()
}

fn remove_file(path: &Path) -> Result<(), WorkspaceError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(WorkspaceError::new(
            "layout_io",
            format!("cannot remove {}: {e}", path.display()),
        )),
    }
}

fn not_found(name: &str) -> WorkspaceError {
    WorkspaceError::new("layout_not_found", format!("no layout is saved as {name:?}"))
}

/// Reads layout `name`.
///
/// # Errors
///
/// `layout_not_found`, or the file's `layout_*` error.
pub fn read_named_layout(dir: &Path, name: &str) -> Result<(NamedLayout, LayoutDoc), WorkspaceError> {
    let layout = find_named_layout(dir, name).ok_or_else(|| not_found(name))?;
    let doc = crate::editor_layout::read_layout_file(&layout.path)?.ok_or_else(|| not_found(name))?;
    Ok((layout, doc))
}

/// Renames layout `from` to `to`, keeping its flag and contents.
///
/// # Errors
///
/// `layout_not_found`, `bad_layout_name`, `layout_exists` (another layout has the new name) or
/// `layout_io`.
pub fn rename_named_layout(dir: &Path, from: &str, to: &str) -> Result<NamedLayout, WorkspaceError> {
    let old = find_named_layout(dir, from).ok_or_else(|| not_found(from))?;
    let to = check_layout_name(to)?;
    if let Some(other) = find_named_layout(dir, &to)
        && other.path != old.path
    {
        return Err(WorkspaceError::new(
            "layout_exists",
            format!("a layout is already saved as {:?}", other.name),
        ));
    }
    let path = named_layout_path(dir, &to, old.workspace);
    std::fs::rename(&old.path, &path).map_err(|e| {
        WorkspaceError::new("layout_io", format!("cannot rename {}: {e}", old.path.display()))
    })?;
    Ok(NamedLayout {
        name: to,
        workspace: old.workspace,
        path,
    })
}

/// Deletes layout `name`.
///
/// # Errors
///
/// `layout_not_found` or `layout_io`.
pub fn delete_named_layout(dir: &Path, name: &str) -> Result<NamedLayout, WorkspaceError> {
    let layout = find_named_layout(dir, name).ok_or_else(|| not_found(name))?;
    for file in files_named(dir, &layout.name) {
        remove_file(&file.path)?;
    }
    Ok(layout)
}

/// The named layout folder of the world's user config (`None` when the editor keeps no per-user
/// files, as in most tests).
#[must_use]
pub fn layout_dir_of(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<crate::editor_prefs::EditorConfigPaths>()
        .and_then(|paths| paths.dir.as_deref().map(named_layout_dir))
}

// ---------------------------------------------------------------------------------------------
// Workspaces

/// The tool palette and toolbar a workspace activates (ADR-8197).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EditorMode {
    /// General world editing.
    #[default]
    World,
    /// Terrain painting and terrain layers.
    Terrain,
    /// Two-dimensional tile painting.
    Tiles2d,
    /// Animation documents and graphs.
    Animation,
    /// In-game UI design.
    UiDesign,
}

impl EditorMode {
    /// Modes offered by the workspace switcher.
    pub const ALL: &[Self] = &[
        Self::World,
        Self::Terrain,
        Self::Tiles2d,
        Self::Animation,
        Self::UiDesign,
    ];

    /// Stable agent id.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::World => "world",
            Self::Terrain => "terrain",
            Self::Tiles2d => "tiles2d",
            Self::Animation => "animation",
            Self::UiDesign => "ui_design",
        }
    }

    /// Parses a stable agent id.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|mode| mode.id() == id)
    }

    /// User-facing mode name.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::World => "World",
            Self::Terrain => "Terrain",
            Self::Tiles2d => "2D tiles",
            Self::Animation => "Animation",
            Self::UiDesign => "UI design",
        }
    }

    /// Existing panels exposed by this mode's toolbar; the first is its primary palette.
    #[must_use]
    pub const fn panels(self) -> &'static [Panel] {
        match self {
            Self::World => &[
                Panel::Viewport,
                Panel::Outliner,
                Panel::Inspector,
                Panel::WorldSettings,
            ],
            Self::Terrain => &[Panel::Terrain, Panel::Layers, Panel::Biome],
            Self::Tiles2d => &[Panel::Mode2d, Panel::Content, Panel::Inspector],
            Self::Animation => &[
                Panel::AnimGraph,
                Panel::AnimClip,
                Panel::AnimSkeleton,
                Panel::Montage,
            ],
            Self::UiDesign => &[Panel::UiDesigner, Panel::UiInspector, Panel::UiAnim, Panel::Theme],
        }
    }
}

#[derive(Resource, Default)]
struct WorkspaceModes(BTreeMap<String, EditorMode>);

fn activate_mode(world: &mut World, mode: EditorMode) {
    if mode != EditorMode::Terrain {
        crate::terrain_tool::end_stroke(world);
    }
    world
        .get_resource_or_insert_with(crate::terrain_tool::TerrainTool::default)
        .active = mode == EditorMode::Terrain;
    let mut tiles = world.get_resource_or_insert_with(crate::mode2d::Mode2d::default);
    let command = if mode != EditorMode::Tiles2d {
        tiles.end_stroke()
    } else {
        None
    };
    let entering = !tiles.active && mode == EditorMode::Tiles2d;
    tiles.active = mode == EditorMode::Tiles2d;
    tiles.pending_frame = tiles.active && (tiles.pending_frame || entering);
    if let Some(command) = command {
        let mut undo = world.get_resource_or_insert_with(crate::command::UndoStack::new);
        undo.push(Box::new(command));
        undo.seal();
    }
}

/// A built-in workspace: its dock is `[outliner | main over bottom | inspector]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BuiltInWorkspace {
    /// Stable id (`editor.workspace_set { id }`, palette `workspace.<id>`).
    pub id: &'static str,
    /// The switcher's label.
    pub title: &'static str,
    /// What it is for, for tooltips.
    pub description: &'static str,
    /// The main tab group, the first one shown.
    pub main: &'static [Panel],
    /// The bottom tab group (empty for none).
    pub bottom: &'static [Panel],
    /// Whether switching to it also opens the profiler window.
    pub opens_profiler: bool,
    /// Default tool mode; absent selects World.
    pub mode: Option<EditorMode>,
}

/// The built-in workspaces, in switcher order. Level is the built-in layout itself.
pub const BUILT_IN_WORKSPACES: &[BuiltInWorkspace] = &[
    BuiltInWorkspace {
        id: LEVEL_WORKSPACE,
        title: "Level",
        description: "Level editing: the built-in layout",
        main: &[Panel::Viewport],
        bottom: &[],
        opens_profiler: false,
        mode: Some(EditorMode::World),
    },
    BuiltInWorkspace {
        id: "animation",
        title: "Animation",
        description: "Animation graphs, clips, skeletons, montages, motion matching and ragdolls",
        main: &[Panel::AnimGraph, Panel::Viewport],
        bottom: &[
            Panel::AnimClip,
            Panel::AnimSkeleton,
            Panel::Montage,
            Panel::MotionMatching,
            Panel::Ragdoll,
            Panel::Content,
            Panel::OutputLog,
        ],
        opens_profiler: false,
        mode: Some(EditorMode::Animation),
    },
    BuiltInWorkspace {
        id: "materials",
        title: "Materials",
        description: "Material graphs, material instances and look development",
        main: &[Panel::MaterialGraph, Panel::Viewport],
        bottom: &[Panel::Material, Panel::LookDev, Panel::Content, Panel::OutputLog],
        opens_profiler: false,
        mode: Some(EditorMode::World),
    },
    BuiltInWorkspace {
        id: "ui",
        title: "UI",
        description: "The in-game UI designer, its animations, inspector, theme and text tables",
        main: &[Panel::UiDesigner, Panel::Viewport],
        bottom: &[
            Panel::UiAnim,
            Panel::UiInspector,
            Panel::Theme,
            Panel::Localization,
            Panel::Content,
            Panel::OutputLog,
        ],
        opens_profiler: false,
        mode: Some(EditorMode::UiDesign),
    },
    BuiltInWorkspace {
        id: "world",
        title: "World",
        description: "Terrain, procedural graphs, the 2D tile mode, layers and world settings",
        main: &[Panel::Viewport],
        bottom: &[
            Panel::Terrain,
            Panel::ProcGraph,
            Panel::Mode2d,
            Panel::Layers,
            Panel::WorldSettings,
            Panel::Content,
            Panel::OutputLog,
        ],
        opens_profiler: false,
        mode: Some(EditorMode::World),
    },
    BuiltInWorkspace {
        id: "cinematics",
        title: "Cinematics",
        description: "Shots, sequence curves, cameras and video",
        main: &[Panel::Viewport],
        bottom: &[
            Panel::Shots,
            Panel::SeqCurves,
            Panel::Cameras,
            Panel::Video,
            Panel::Content,
            Panel::OutputLog,
        ],
        opens_profiler: false,
        mode: Some(EditorMode::World),
    },
    BuiltInWorkspace {
        id: "debug",
        title: "Debug",
        description: "The profiler, the log, problems, tests and network state",
        main: &[Panel::Viewport],
        bottom: &[
            Panel::OutputLog,
            Panel::Problems,
            Panel::Tests,
            Panel::Net,
            Panel::SizeMap,
        ],
        opens_profiler: true,
        mode: Some(EditorMode::World),
    },
    BuiltInWorkspace {
        id: "terrain",
        title: "Terrain",
        description: "Terrain painting and layers",
        main: &[Panel::Viewport],
        bottom: &[Panel::Terrain, Panel::Layers, Panel::Content],
        opens_profiler: false,
        mode: Some(EditorMode::Terrain),
    },
    BuiltInWorkspace {
        id: "tiles2d",
        title: "2D tiles",
        description: "Two-dimensional tile painting",
        main: &[Panel::Viewport],
        bottom: &[Panel::Mode2d, Panel::Content],
        opens_profiler: false,
        mode: Some(EditorMode::Tiles2d),
    },
];

/// The built-in workspace `id`.
#[must_use]
pub fn built_in_workspace(id: &str) -> Option<&'static BuiltInWorkspace> {
    BUILT_IN_WORKSPACES.iter().find(|w| w.id == id)
}

impl BuiltInWorkspace {
    /// Every panel the workspace's dock opens.
    #[must_use]
    pub fn panels(&self) -> Vec<Panel> {
        if self.id == LEVEL_WORKSPACE {
            return crate::editor_layout::default_dock()
                .iter_all_tabs()
                .map(|(_, tab)| *tab)
                .collect();
        }
        let mut out = vec![Panel::Outliner, Panel::Inspector];
        out.extend_from_slice(self.main);
        out.extend_from_slice(self.bottom);
        out
    }

    /// The workspace's dock as first shown.
    #[must_use]
    pub fn dock(&self) -> DockState<Panel> {
        if self.id == LEVEL_WORKSPACE {
            return crate::editor_layout::default_dock();
        }
        let mut dock = DockState::new(self.main.to_vec());
        let tree = dock.main_surface_mut();
        // The splits return [old, new]; `fraction` is the left/top child's share (as the
        // built-in layout).
        let [center, _outliner] = tree.split_left(NodeIndex::root(), 0.17, vec![Panel::Outliner]);
        let [center, _inspector] = tree.split_right(center, 0.76, vec![Panel::Inspector]);
        if !self.bottom.is_empty() {
            let _ = tree.split_below(center, 0.62, self.bottom.to_vec());
        }
        dock
    }
}

/// One entry of the workspace switcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEntry {
    /// The id (`level`, ..., or `user:<name>`).
    pub id: String,
    /// The label.
    pub title: String,
    /// Built in, or the user's.
    pub built_in: bool,
    /// Effective tool mode for this session.
    pub mode: EditorMode,
    /// Registering extension id, when supplied by an extension.
    pub extension: Option<String>,
}

/// The switcher label of a workspace id, without listing the layouts folder: a built-in's title,
/// a user workspace's name (`user:<name>`), else the id itself. Matches [`workspace_entries`]'
/// `title` for every id it lists.
#[must_use]
pub fn workspace_label(id: &str) -> String {
    BUILT_IN_WORKSPACES
        .iter()
        .find(|w| w.id == id)
        .map(|w| w.title.to_owned())
        .or_else(|| id.strip_prefix(USER_WORKSPACE_PREFIX).map(str::to_owned))
        .unwrap_or_else(|| id.to_owned())
}

/// The built-in workspaces, then the user's workspace layouts in `dir`.
#[must_use]
pub fn workspace_entries(dir: Option<&Path>) -> Vec<WorkspaceEntry> {
    let mut out: Vec<WorkspaceEntry> = BUILT_IN_WORKSPACES
        .iter()
        .map(|w| WorkspaceEntry {
            id: w.id.to_owned(),
            title: w.title.to_owned(),
            built_in: true,
            mode: w.mode.unwrap_or_default(),
            extension: None,
        })
        .collect();
    if let Some(dir) = dir {
        out.extend(
            list_named_layouts(dir)
                .into_iter()
                .filter(|l| l.workspace)
                .map(|l| WorkspaceEntry {
                    id: format!("{USER_WORKSPACE_PREFIX}{}", l.name),
                    title: l.name,
                    built_in: false,
                    mode: EditorMode::World,
                    extension: None,
                }),
        );
    }
    out
}

/// Workspaces currently available in `world`, including live extension registrations and
/// session-local mode overrides. User files are read only when this list is requested.
#[must_use]
pub fn workspace_entries_in(world: &World) -> Vec<WorkspaceEntry> {
    let dir = layout_dir_of(world);
    let mut entries = workspace_entries(dir.as_deref());
    entries.extend(
        crate::extension::context::live_workspaces(world).map(|(owner, desc)| WorkspaceEntry {
            id: format!("extension:{}", desc.id),
            title: desc.title.clone(),
            built_in: false,
            mode: desc.mode.unwrap_or_default(),
            extension: Some(owner.to_owned()),
        }),
    );
    if let Some(overrides) = world.get_resource::<WorkspaceModes>() {
        for entry in &mut entries {
            if let Some(mode) = overrides.0.get(&entry.id) {
                entry.mode = *mode;
            }
        }
    }
    entries
}

fn workspace_mode(world: &World, id: &str) -> EditorMode {
    world
        .get_resource::<WorkspaceModes>()
        .and_then(|m| m.0.get(id))
        .copied()
        .or_else(|| built_in_workspace(id).and_then(|w| w.mode))
        .or_else(|| {
            id.strip_prefix("extension:").and_then(|id| {
                crate::extension::context::live_workspaces(world)
                    .find(|(_, w)| w.id == id)
                    .and_then(|(_, w)| w.mode)
            })
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Requests and the shell's view state

/// A dock view action for the shell's next frame (GUI controls, palette commands, keys and agent
/// methods all queue these, so every route runs the same code).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewRequest {
    /// Switch workspace (`level`, ..., `user:<name>`).
    SetWorkspace(String),
    /// Switch workspace and remember a session-local tool mode override.
    SetWorkspaceMode {
        /// Workspace id.
        id: String,
        /// The tool mode to activate.
        mode: EditorMode,
    },
    /// Merge the source area into a neighboring tab stack.
    Join {
        /// Any tab in the source area.
        from: Panel,
        /// Any tab in the destination area.
        into: Panel,
    },
    /// Exchange the complete tab stacks of two areas.
    Swap {
        /// Any tab in the first area.
        from: Panel,
        /// Any tab in the second area.
        other: Panel,
    },
    /// Maximize a tab, or restore when one is maximized. `None` takes the focused tab group's
    /// shown tab (the Ctrl+Space key path passes the hovered group's tab as `Some`).
    ToggleMaximize(Option<Panel>),
    /// Enter or leave the full-screen viewport.
    ToggleFullscreen,
    /// Leave the full-screen viewport (Escape); nothing when not in it.
    LeaveFullscreen,
    /// Add `panel` to the tab group holding `near` (the focused group when `None`), moving it when
    /// it is open elsewhere.
    AddTab {
        /// The panel to add.
        panel: Panel,
        /// A tab of the target group.
        near: Option<Panel>,
    },
    /// Put `to` in place of the tab `from`.
    Replace {
        /// The tab replaced.
        from: Panel,
        /// The panel shown instead.
        to: Panel,
    },
    /// Bring an open tab forward.
    Focus(Panel),
}

/// Queued [`ViewRequest`]s (the shell drains them each frame).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewRequests {
    /// In arrival order.
    pub pending: Vec<ViewRequest>,
}

impl ViewRequests {
    /// Queues `request` in `world`.
    pub fn push(world: &mut World, request: ViewRequest) {
        world
            .get_resource_or_insert_with(Self::default)
            .pending
            .push(request);
    }
}

/// The dock view state the shell publishes when it changes (for agents, menus and the toolbar).
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct ViewStatus {
    /// The current workspace id.
    pub workspace: String,
    /// Active workspace tool mode.
    pub mode: EditorMode,
    /// The maximized tab's panel id, if one is.
    pub maximized: Option<&'static str>,
    /// Whether the full-screen viewport shows (the editor chrome is hidden).
    pub fullscreen: bool,
    /// Workspaces with a parked dock this session.
    pub parked: Vec<String>,
}

impl Default for ViewStatus {
    fn default() -> Self {
        Self {
            workspace: LEVEL_WORKSPACE.to_owned(),
            mode: EditorMode::World,
            maximized: None,
            fullscreen: false,
            parked: Vec::new(),
        }
    }
}

/// A maximized tab and the dock it replaced.
#[derive(Debug, Clone)]
struct Maximized {
    saved: DockState<Panel>,
    panel: Panel,
    fullscreen: bool,
}

/// The shell's workspace, maximize and drag-dwell state.
#[derive(Debug, Clone)]
pub struct DockViews {
    workspace: String,
    mode: EditorMode,
    parked: BTreeMap<String, DockState<Panel>>,
    maximized: Option<Maximized>,
    /// The feature profile generation last applied to the dock.
    profile_seen: u64,
    /// Tabs the feature profile closed, with a tab of the group they were in.
    stashed: Vec<(Panel, Option<Panel>)>,
}

impl Default for DockViews {
    fn default() -> Self {
        Self {
            workspace: LEVEL_WORKSPACE.to_owned(),
            mode: EditorMode::World,
            parked: BTreeMap::new(),
            maximized: None,
            profile_seen: 0,
            stashed: Vec::new(),
        }
    }
}

impl DockViews {
    /// The current workspace id.
    #[must_use]
    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    /// Whether the full-screen viewport shows (the shell then skips its menus, toolbars and status
    /// bar).
    #[must_use]
    pub fn chrome_hidden(&self) -> bool {
        self.maximized.as_ref().is_some_and(|m| m.fullscreen)
    }

    /// The maximized panel, if any.
    #[must_use]
    pub fn maximized(&self) -> Option<Panel> {
        self.maximized.as_ref().map(|m| m.panel)
    }

    /// The dock to save and report: the one a maximized tab replaced, else `dock`.
    #[must_use]
    pub fn base_dock<'a>(&'a self, dock: &'a DockState<Panel>) -> &'a DockState<Panel> {
        self.maximized.as_ref().map_or(dock, |m| &m.saved)
    }

    /// The dock a maximized tab replaced, else `dock`, to edit: background tab changes (an
    /// extension's panel coming or going) land there, so the one-tab dock stays one tab and a
    /// restore keeps them (review sprint-v06-rev-late-k #10).
    pub fn base_dock_mut<'a>(&'a mut self, dock: &'a mut DockState<Panel>) -> &'a mut DockState<Panel> {
        match self.maximized.as_mut() {
            Some(m) => &mut m.saved,
            None => dock,
        }
    }

    /// Drops the maximize state without restoring (the shell replaced the whole layout).
    pub fn forget_maximize(&mut self) {
        self.maximized = None;
    }

    /// Puts the dock a maximized tab replaced back. Returns whether one was.
    pub fn restore(&mut self, dock: &mut DockState<Panel>) -> bool {
        match self.maximized.take() {
            Some(m) => {
                *dock = m.saved;
                true
            }
            None => false,
        }
    }

    /// The status to publish.
    #[must_use]
    pub fn status(&self) -> ViewStatus {
        ViewStatus {
            workspace: self.workspace.clone(),
            mode: self.mode,
            maximized: self.maximized.as_ref().map(|m| m.panel.id()),
            fullscreen: self.chrome_hidden(),
            parked: self.parked.keys().cloned().collect(),
        }
    }

    /// Publishes [`ViewStatus`] when it changed.
    pub fn publish(&self, world: &mut World) {
        // Compared field by field first, so an unchanged state allocates nothing per frame.
        let same = world.get_resource::<ViewStatus>().is_some_and(|s| {
            s.workspace == self.workspace
                && s.mode == self.mode
                && s.maximized == self.maximized.as_ref().map(|m| m.panel.id())
                && s.fullscreen == self.chrome_hidden()
                && s.parked.len() == self.parked.len()
                && s.parked.iter().zip(self.parked.keys()).all(|(a, b)| a == b)
        });
        if !same {
            world.insert_resource(self.status());
        }
    }

    /// Performs the queued [`ViewRequest`]s on `dock`. Returns a status line for the last request
    /// that produced one.
    pub fn apply_requests(&mut self, dock: &mut DockState<Panel>, world: &mut World) -> Option<String> {
        let requests = world.remove_resource::<ViewRequests>()?;
        let mut toast = None;
        for request in requests.pending {
            match self.apply(dock, world, request) {
                Ok(Some(line)) => toast = Some(line),
                Ok(None) => {}
                Err(err) => toast = Some(err.message),
            }
        }
        toast
    }

    /// Performs one request.
    ///
    /// # Errors
    ///
    /// The request's [`WorkspaceError`].
    pub fn apply(
        &mut self,
        dock: &mut DockState<Panel>,
        world: &mut World,
        request: ViewRequest,
    ) -> Result<Option<String>, WorkspaceError> {
        match request {
            ViewRequest::SetWorkspace(id) => self.set_workspace(dock, world, &id).map(Some),
            ViewRequest::SetWorkspaceMode { id, mode } => {
                let result = self.set_workspace(dock, world, &id)?;
                world
                    .get_resource_or_insert_with(WorkspaceModes::default)
                    .0
                    .insert(id, mode);
                self.mode = mode;
                activate_mode(world, mode);
                if let Some(panel) = mode.panels().first().copied() {
                    if let Some(path) = dock.find_tab(&panel) {
                        let _ = dock.set_active_tab(path);
                    } else {
                        add_tab(dock, panel, None)?;
                    }
                }
                Ok(Some(result))
            }
            ViewRequest::Join { from, into } => {
                let mut next = self.base_dock(dock).clone();
                join_areas(&mut next, from, into)?;
                self.forget_maximize();
                *dock = next;
                Ok(Some("Areas joined".into()))
            }
            ViewRequest::Swap { from, other } => {
                let mut next = self.base_dock(dock).clone();
                swap_areas(&mut next, from, other)?;
                self.forget_maximize();
                *dock = next;
                Ok(Some("Areas swapped".into()))
            }
            ViewRequest::ToggleMaximize(panel) => {
                if self.restore(dock) {
                    return Ok(Some("Panel restored".to_owned()));
                }
                // The named tab (the key path names the hovered group's), else the focused one,
                // else the viewport, else any. A menu click or the palette leaves the pointer over
                // some dock group, so only the key path may use the hover (review
                // sprint-v06-rev-late-k #9).
                let panel = panel
                    .or_else(|| focused_tab(dock))
                    .or_else(|| dock.find_tab(&Panel::Viewport).map(|_| Panel::Viewport))
                    .or_else(|| dock.iter_all_tabs().next().map(|(_, tab)| *tab))
                    .ok_or_else(|| WorkspaceError::new("panel_not_open", "no tab to maximize"))?;
                self.maximize(dock, panel, false)?;
                Ok(Some(format!("{} maximized (Ctrl+Space restores)", panel.title())))
            }
            ViewRequest::ToggleFullscreen => {
                let was_fullscreen = self.chrome_hidden();
                self.restore(dock);
                if was_fullscreen {
                    return Ok(Some("Left the full-screen viewport".to_owned()));
                }
                self.maximize(dock, Panel::Viewport, true)?;
                Ok(Some("Full-screen viewport (F11 or Escape leaves)".to_owned()))
            }
            ViewRequest::LeaveFullscreen => {
                if self.chrome_hidden() {
                    self.restore(dock);
                    return Ok(Some("Left the full-screen viewport".to_owned()));
                }
                Ok(None)
            }
            ViewRequest::AddTab { panel, near } => {
                self.restore(dock);
                add_tab(dock, panel, near)?;
                Ok(Some(format!("{} tab added", panel.title())))
            }
            ViewRequest::Replace { from, to } => {
                self.restore(dock);
                replace_tab(dock, from, to)?;
                Ok(Some(format!("{} replaced by {}", from.title(), to.title())))
            }
            ViewRequest::Focus(panel) => {
                let path = dock.find_tab(&panel).ok_or_else(|| panel_not_open(panel))?;
                let _ = dock.set_active_tab(path);
                dock.set_focused_node_and_surface(path.node_path());
                // A tab picked from the overflow list scrolls into its bar (ADR-7902).
                if let Some(strips) = world.get_resource::<TabStrips>()
                    && let Some(tab_rect) = strips.rect(panel)
                    && let Ok(leaf) = dock.leaf_mut(path.node_path())
                {
                    let bar = tab_bar_rect(leaf.rect, strips.bar_height);
                    leaf.scroll = scroll_to_reveal(leaf.scroll, tab_rect, visible_tab_strip(bar));
                }
                Ok(None)
            }
        }
    }

    /// Shows `panel` alone in the dock area, keeping the current dock to restore.
    fn maximize(
        &mut self,
        dock: &mut DockState<Panel>,
        panel: Panel,
        fullscreen: bool,
    ) -> Result<(), WorkspaceError> {
        // Full screen shows the viewport even when its tab was closed.
        if !fullscreen && dock.find_tab(&panel).is_none() {
            return Err(panel_not_open(panel));
        }
        let saved = std::mem::replace(dock, DockState::new(vec![panel]));
        self.maximized = Some(Maximized {
            saved,
            panel,
            fullscreen,
        });
        Ok(())
    }

    /// Parks the shown dock under the current workspace and shows workspace `id`.
    fn set_workspace(
        &mut self,
        dock: &mut DockState<Panel>,
        world: &mut World,
        id: &str,
    ) -> Result<String, WorkspaceError> {
        let mode = workspace_mode(world, id);
        let fresh = !self.parked.contains_key(id) && id != self.workspace;
        let (next, title, profiler) = if let Some(extension_id) = id.strip_prefix("extension:") {
            // Validate the live registration before considering a parked copy.
            let desc = crate::extension::context::live_workspaces(world)
                .find(|(_, desc)| desc.id == extension_id)
                .map(|(_, desc)| desc.clone())
                .ok_or_else(|| unknown_workspace(id))?;
            let next = match self.parked.remove(id) {
                Some(parked) => parked,
                None => desc.layout.to_dock()?.0,
            };
            (next, desc.title, false)
        } else if let Some(name) = id.strip_prefix(USER_WORKSPACE_PREFIX) {
            let parked = self.parked.remove(id);
            let next = match parked {
                Some(dock) => dock,
                None => {
                    let dir = layout_dir_of(world).ok_or_else(|| {
                        WorkspaceError::new("no_user_config", "this editor keeps no per-user files")
                    })?;
                    let (layout, doc) = read_named_layout(&dir, name).map_err(|_| unknown_workspace(id))?;
                    if !layout.workspace {
                        return Err(unknown_workspace(id));
                    }
                    doc.to_dock()?.0
                }
            };
            (next, name.to_owned(), false)
        } else {
            let built_in = built_in_workspace(id).ok_or_else(|| unknown_workspace(id))?;
            let next = self.parked.remove(id).unwrap_or_else(|| built_in.dock());
            (next, built_in.title.to_owned(), built_in.opens_profiler)
        };
        self.restore(dock);
        self.mode = mode;
        activate_mode(world, mode);
        if id == self.workspace {
            // Re-selecting the current workspace keeps its dock.
            self.parked.remove(id);
            return Ok(format!("{title} workspace"));
        }
        let left = std::mem::replace(dock, next);
        self.parked
            .insert(std::mem::replace(&mut self.workspace, id.to_owned()), left);
        if fresh && let Some(panel) = mode.panels().first().copied() {
            if let Some(path) = dock.find_tab(&panel) {
                let _ = dock.set_active_tab(path);
            } else {
                add_tab(dock, panel, None)?;
            }
        }
        // The feature profile applies to the new dock on the next sync.
        self.profile_seen = 0;
        if profiler {
            world
                .get_resource_or_insert_with(crate::profiler_panel::ProfilerPanel::default)
                .open = true;
        }
        Ok(format!("{title} workspace"))
    }

    /// Applies the open project's feature profile to the dock when it changed: hidden panels'
    /// tabs close (remembered), and tabs it closed before come back when shown again.
    pub fn sync_profile(&mut self, dock: &mut DockState<Panel>, world: &mut World) {
        crate::feature_profiles::sync(world);
        let Some(active) = world.get_resource::<crate::feature_profiles::ActiveFeatureProfile>() else {
            return;
        };
        if active.generation == self.profile_seen {
            return;
        }
        self.profile_seen = active.generation;
        let profile = active.profile.clone();
        self.restore(dock);
        for (panel, near) in std::mem::take(&mut self.stashed) {
            if profile.hides_panel(panel) {
                self.stashed.push((panel, near));
            } else if dock.find_tab(&panel).is_none() {
                let near = near.filter(|n| dock.find_tab(n).is_some());
                let _ = add_tab(dock, panel, near);
            }
        }
        let hidden: Vec<Panel> = dock
            .iter_all_tabs()
            .map(|(_, tab)| *tab)
            .filter(|tab| profile.hides_panel(*tab))
            .collect();
        for panel in hidden {
            let Some(path) = dock.find_tab(&panel) else {
                continue;
            };
            let near = dock.leaf(path.node_path()).ok().and_then(|leaf| {
                leaf.tabs
                    .iter()
                    .copied()
                    .find(|t| *t != panel && !profile.hides_panel(*t))
            });
            dock.remove_tab(path);
            self.stashed.push((panel, near));
        }
    }
}

/// The tab an item is being dragged over and since when (`(tab, first frame, last frame)` in
/// egui's `cumulative_frame_nr`).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TabDwell(pub Option<(Panel, u64, u64)>);

impl TabDwell {
    /// Notes that an item is dragged over `tab` in egui frame `frame`. Returns `true` once, on the
    /// frame the dwell reaches [`DWELL_FRAMES`] (the caller brings the tab forward). Two passes of
    /// one frame count once; a frame without a hover restarts it.
    pub fn note(&mut self, tab: Panel, frame: u64) -> bool {
        match &mut self.0 {
            Some((panel, since, last)) if *panel == tab && (*last == frame || *last + 1 == frame) => {
                let before = *last - *since;
                *last = frame;
                let now = frame - *since;
                before < DWELL_FRAMES && now >= DWELL_FRAMES
            }
            _ => {
                self.0 = Some((tab, frame, frame));
                false
            }
        }
    }
}

/// A tab button's hook: while an item (asset, entity) is dragged and the pointer is on `tab`'s
/// button, count frames; after [`DWELL_FRAMES`] the tab comes forward.
pub fn tab_drag_dwell(world: &mut World, tab: Panel, response: &egui::Response) {
    if !egui::DragAndDrop::has_any_payload(&response.ctx) || !response.contains_pointer() {
        return;
    }
    let frame = response.ctx.cumulative_frame_nr();
    // Keep frames coming while the pointer rests on the tab.
    response.ctx.request_repaint();
    if world
        .get_resource_or_insert_with(TabDwell::default)
        .note(tab, frame)
    {
        ViewRequests::push(world, ViewRequest::Focus(tab));
    }
}

fn unknown_workspace(id: &str) -> WorkspaceError {
    WorkspaceError::new(
        "unknown_workspace",
        format!("no workspace has the id {id:?} (editor.workspace_list lists them)"),
    )
}

fn panel_not_open(panel: Panel) -> WorkspaceError {
    WorkspaceError::new("panel_not_open", format!("the {} panel has no tab", panel.id()))
}

fn bad_area() -> WorkspaceError {
    WorkspaceError::new(
        "bad_pane_request",
        "choose two different areas; joined areas must share an edge on the same surface",
    )
}

// Original normalized split-tree geometry, derived from the public egui_dock API (ADR-8197).
// Painted rectangles are unavailable before a frame and include separators; topology is stable.
fn area_rect(dock: &DockState<Panel>, path: egui_dock::NodePath) -> Option<[f64; 4]> {
    let mut route = Vec::new();
    let mut child = path.node;
    while let Some(parent) = child.parent() {
        route.push((parent, child.is_left()));
        child = parent;
    }
    let mut rect = [0.0, 0.0, 1.0, 1.0];
    for (parent, first) in route.into_iter().rev() {
        let node = dock.node(egui_dock::NodePath::new(path.surface, parent)).ok()?;
        let (axis, fraction) = match node {
            egui_dock::Node::Horizontal(split) => (0, f64::from(split.fraction)),
            egui_dock::Node::Vertical(split) => (1, f64::from(split.fraction)),
            _ => return None,
        };
        if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) {
            return None;
        }
        let cut = rect[axis] + (rect[axis + 2] - rect[axis]) * fraction;
        rect[if first { axis + 2 } else { axis }] = cut;
    }
    Some(rect)
}

fn neighboring(dock: &DockState<Panel>, a: egui_dock::NodePath, b: egui_dock::NodePath) -> bool {
    if a == b || a.surface != b.surface {
        return false;
    }
    let (Some(a), Some(b)) = (area_rect(dock, a), area_rect(dock, b)) else {
        return false;
    };
    if a[0] >= a[2] || a[1] >= a[3] || b[0] >= b[2] || b[1] >= b[3] {
        return false;
    }
    ((a[2] == b[0] || b[2] == a[0]) && a[1].max(b[1]) < a[3].min(b[3]))
        || ((a[3] == b[1] || b[3] == a[1]) && a[0].max(b[0]) < a[2].min(b[2]))
}

/// Joins two neighboring areas atomically, appending source tabs and preserving the target's
/// active tab. The emptied source leaf collapses through the dock's public API.
///
/// # Errors
///
/// `panel_not_open` or `bad_pane_request`; a refused operation leaves `dock` unchanged.
pub fn join_areas(dock: &mut DockState<Panel>, from: Panel, into: Panel) -> Result<(), WorkspaceError> {
    let _span = tracing::debug_span!("editor.layout_join").entered();
    let source = dock
        .find_tab(&from)
        .ok_or_else(|| panel_not_open(from))?
        .node_path();
    let target = dock
        .find_tab(&into)
        .ok_or_else(|| panel_not_open(into))?
        .node_path();
    if !neighboring(dock, source, target) {
        return Err(bad_area());
    }
    let tabs = dock.leaf(source).map_err(|_| bad_area())?.tabs.clone();
    let mut next = dock.clone();
    next.remove_leaf(source);
    // Collapsing can renumber the target, so resolve it again by its surviving tab.
    let target = next
        .find_tab(&into)
        .ok_or_else(|| panel_not_open(into))?
        .node_path();
    next.leaf_mut(target).map_err(|_| bad_area())?.tabs.extend(tabs);
    next.set_focused_node_and_surface(target);
    *dock = next;
    Ok(())
}

/// Exchanges complete tab stacks, active tabs and scroll positions while keeping both areas'
/// geometry. This is one atomic layout edit.
///
/// # Errors
///
/// `panel_not_open` or `bad_pane_request`; a refused operation leaves `dock` unchanged.
pub fn swap_areas(dock: &mut DockState<Panel>, from: Panel, other: Panel) -> Result<(), WorkspaceError> {
    let _span = tracing::debug_span!("editor.layout_swap").entered();
    let a = dock
        .find_tab(&from)
        .ok_or_else(|| panel_not_open(from))?
        .node_path();
    let b = dock
        .find_tab(&other)
        .ok_or_else(|| panel_not_open(other))?
        .node_path();
    if a == b {
        return Err(bad_area());
    }
    let first = dock.leaf(a).map_err(|_| bad_area())?.clone();
    let second = dock.leaf(b).map_err(|_| bad_area())?.clone();
    let mut next = dock.clone();
    for (path, source) in [(a, second), (b, first)] {
        let leaf = next.leaf_mut(path).map_err(|_| bad_area())?;
        leaf.tabs = source.tabs;
        leaf.active = source.active;
        leaf.scroll = source.scroll;
    }
    next.set_focused_node_and_surface(b);
    *dock = next;
    Ok(())
}

#[derive(Resource, Clone)]
struct AreaSnapshot(DockState<Panel>);

/// The focused tab group's shown tab.
#[must_use]
pub fn focused_tab(dock: &DockState<Panel>) -> Option<Panel> {
    let path = dock.focused_leaf()?;
    let leaf = dock.leaf(path).ok()?;
    leaf.tabs.get(leaf.active.0).copied()
}

/// The shown tab of the tab group under `pos` (a floating window over the main surface wins).
#[must_use]
pub fn tab_at(dock: &DockState<Panel>, pos: egui::Pos2) -> Option<Panel> {
    let mut hit = None;
    for (path, leaf) in dock.iter_leaves() {
        if leaf.rect.is_finite() && leaf.rect.contains(pos) {
            let shown = leaf.tabs.get(leaf.active.0).copied();
            if !path.surface.is_main() {
                return shown;
            }
            hit = hit.or(shown);
        }
    }
    hit
}

/// Adds `panel` to the tab group of `near` (else the focused group) and brings it forward; a
/// panel open elsewhere moves.
///
/// # Errors
///
/// `panel_not_open` when `near` has no tab, `bad_pane_request` when `near` is `panel` itself.
pub fn add_tab(dock: &mut DockState<Panel>, panel: Panel, near: Option<Panel>) -> Result<(), WorkspaceError> {
    if near == Some(panel) {
        // Already in that group: just bring it forward.
        let path = dock.find_tab(&panel).ok_or_else(|| panel_not_open(panel))?;
        let _ = dock.set_active_tab(path);
        return Ok(());
    }
    if let Some(near) = near
        && dock.find_tab(&near).is_none()
    {
        return Err(panel_not_open(near));
    }
    if let Some(path) = dock.find_tab(&panel) {
        dock.remove_tab(path);
    }
    match near.and_then(|near| dock.find_tab(&near)) {
        Some(near_path) => {
            let node = near_path.node_path();
            if let Ok(leaf) = dock.leaf_mut(node) {
                leaf.tabs.push(panel);
                leaf.active = egui_dock::TabIndex(leaf.tabs.len() - 1);
            }
            dock.set_focused_node_and_surface(node);
        }
        None => dock.push_to_focused_leaf(panel),
    }
    Ok(())
}

/// Puts `to` where the tab `from` is and shows it; `to` open elsewhere moves (it is never in two
/// places).
///
/// # Errors
///
/// `panel_not_open` when `from` has no tab.
pub fn replace_tab(dock: &mut DockState<Panel>, from: Panel, to: Panel) -> Result<(), WorkspaceError> {
    if dock.find_tab(&from).is_none() {
        return Err(panel_not_open(from));
    }
    if from == to {
        return Ok(());
    }
    if let Some(path) = dock.find_tab(&to) {
        dock.remove_tab(path);
    }
    let path = dock.find_tab(&from).ok_or_else(|| panel_not_open(from))?;
    if let Ok(leaf) = dock.leaf_mut(path.node_path()) {
        if let Some(slot) = leaf.tabs.get_mut(path.tab.0) {
            *slot = to;
        }
        leaf.active = path.tab;
    }
    dock.set_focused_node_and_surface(path.node_path());
    Ok(())
}

/// What a pressed Escape does with the full-screen viewport in the picture: a focused panel keeps
/// it, a running play session stops first (full screen stays), then full screen ends, then the
/// editor asks to close.
#[must_use]
pub fn escape_action_full(
    session_active: bool,
    panel_wants_keyboard: bool,
    fullscreen: bool,
) -> crate::shell::EscapeAction {
    use crate::shell::EscapeAction;
    match crate::shell::escape_action(session_active, panel_wants_keyboard) {
        EscapeAction::Close if fullscreen => EscapeAction::LeaveFullscreen,
        other => other,
    }
}

/// Whether the world shows the full-screen viewport (for the window host's Escape).
#[must_use]
pub fn fullscreen_in(world: &World) -> bool {
    world.get_resource::<ViewStatus>().is_some_and(|s| s.fullscreen)
}

/// The built-in Ctrl+Space and F11 when the user has not rebound them (a user binding runs
/// through the palette's shortcut dispatch). Not while a text field has the keyboard.
/// `hovered` is the shown tab of the group under the pointer: Ctrl+Space maximizes it (else the
/// focused tab); the menu, palette and agent routes always take the focused tab.
pub fn handle_view_keys(ctx: &egui::Context, world: &mut World, hovered: Option<Panel>) {
    if ctx.text_edit_focused() {
        return;
    }
    for (id, request) in [
        (MAXIMIZE_COMMAND, ViewRequest::ToggleMaximize(hovered)),
        (FULLSCREEN_COMMAND, ViewRequest::ToggleFullscreen),
    ] {
        let user_bound = world
            .get_resource::<crate::command_palette::CommandKeymap>()
            .is_some_and(|k| k.bindings.contains_key(id));
        if user_bound {
            continue;
        }
        let chord = if id == MAXIMIZE_COMMAND {
            MAXIMIZE_CHORD
        } else {
            FULLSCREEN_CHORD
        };
        if ctx.input_mut(|i| i.consume_shortcut(&chord.shortcut())) {
            ViewRequests::push(world, request);
        }
    }
}

/// Ctrl+Space, the built-in chord of [`MAXIMIZE_COMMAND`] (the palette lists the same text).
pub const MAXIMIZE_CHORD: crate::command_palette::KeyChord = crate::command_palette::KeyChord {
    command: true,
    shift: false,
    alt: false,
    key: egui::Key::Space,
};

/// F11, the built-in chord of [`FULLSCREEN_COMMAND`].
pub const FULLSCREEN_CHORD: crate::command_palette::KeyChord = crate::command_palette::KeyChord {
    command: false,
    shift: false,
    alt: false,
    key: egui::Key::F11,
};

/// The palette commands of this module (`editor.command { run }`): maximize, full screen and
/// `workspace.<id>`. Returns the status line, `None` for another id.
pub fn run_command(world: &mut World, id: &str) -> Option<String> {
    let (request, line) = match id {
        MAXIMIZE_COMMAND => (
            ViewRequest::ToggleMaximize(None),
            "Maximize or restore the panel".to_owned(),
        ),
        FULLSCREEN_COMMAND => (ViewRequest::ToggleFullscreen, "Full-screen viewport".to_owned()),
        _ => {
            let workspace = built_in_workspace(id.strip_prefix(WORKSPACE_COMMAND_PREFIX)?)?;
            (
                ViewRequest::SetWorkspace(workspace.id.to_owned()),
                format!("{} workspace", workspace.title),
            )
        }
    };
    ViewRequests::push(world, request);
    Some(line)
}

// ---------------------------------------------------------------------------------------------
// GUI: the toolbar switcher, the View menu, the add-tab popup and the tab context menu

/// The workspace switcher (the main toolbar's right end and View ▸ Workspace): one selectable
/// row per workspace; picking one queues the switch.
pub fn workspace_switcher(ui: &mut egui::Ui, world: &mut World) {
    let current = world
        .get_resource::<ViewStatus>()
        .map_or_else(|| LEVEL_WORKSPACE.to_owned(), |s| s.workspace.clone());
    // The label comes from the id: the layouts folder is listed only while the list is open
    // (bounded per-frame work, ADR-7752 (3); review sprint-v06-rev-late-k #11).
    let shown = current
        .strip_prefix("extension:")
        .and_then(|id| crate::extension::context::live_workspaces(world).find(|(_, desc)| desc.id == id))
        .map_or_else(
            || workspace_label(&current),
            |(_, desc)| format!("{} (extension)", desc.title),
        );
    egui::ComboBox::from_id_salt("editor-workspace-switcher")
        .selected_text(format!("Workspace: {shown}"))
        .show_ui(ui, |ui| {
            for mut entry in workspace_entries_in(world) {
                if entry.extension.is_some() {
                    entry.title.push_str(" (extension)");
                }
                // `editor.workspace_set { id }`.
                if ui
                    .selectable_label(entry.id == current, entry.title.as_str())
                    .clicked()
                {
                    queue_ui(ui, world, ViewRequest::SetWorkspace(entry.id.clone()));
                }
            }
        })
        .response
        .on_hover_text("Switch workspace: a layout and the panels it opens (open documents stay open)")
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, true, "Workspace"));
}

fn queue_ui(ui: &egui::Ui, world: &mut World, request: ViewRequest) {
    if crate::mode2d::first_apply_this_frame(ui.ctx(), ui.id().with(format!("workspace:{request:?}"))) {
        ViewRequests::push(world, request);
    }
}

/// Workspace mode selector and the active mode's panel toolbar. Standard egui controls provide
/// keyboard operation, theme colors and appearance scaling.
pub fn mode_toolbar(ui: &mut egui::Ui, world: &mut World) {
    let status = world.get_resource::<ViewStatus>().cloned().unwrap_or_default();
    ui.horizontal_wrapped(|ui| {
        egui::ComboBox::from_id_salt("workspace-mode")
            .selected_text(format!("Mode: {}", status.mode.title()))
            .show_ui(ui, |ui| {
                for mode in EditorMode::ALL {
                    if crate::agent_parity::action_button_text(
                        ui,
                        &crate::agent_parity::VIEW_WORKSPACE,
                        mode.title(),
                    )
                    .clicked()
                    {
                        queue_ui(
                            ui,
                            world,
                            ViewRequest::SetWorkspaceMode {
                                id: status.workspace.clone(),
                                mode: *mode,
                            },
                        );
                        ui.close();
                    }
                }
            })
            .response
            .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, true, "Editor mode"));
        ui.separator();
        for panel in status.mode.panels() {
            if crate::feature_profiles::panel_hidden(world, *panel) {
                continue;
            }
            if crate::agent_parity::action_button_text(
                ui,
                &crate::agent_parity::WORKSPACE_TOOL,
                panel.title(),
            )
            .clicked()
                && crate::mode2d::first_apply_this_frame(ui.ctx(), ui.id().with(("mode-tool", panel.id())))
            {
                LayoutRequests::push(world, LayoutRequest::Open(*panel));
            }
        }
    });
}

/// The View menu's layout section: Workspace, Layouts (save, switch, rename, delete), Maximize,
/// Full-screen viewport and Panels. `hidden` names panels the feature profile hides. Returns a
/// status line.
pub fn view_menu(ui: &mut egui::Ui, world: &mut World, naming: &mut LayoutNaming) -> Option<String> {
    use crate::agent_parity::{
        VIEW_FULLSCREEN_VIEWPORT, VIEW_MAXIMIZE_PANEL, VIEW_NAMED_LAYOUTS, VIEW_PANELS, VIEW_WORKSPACE,
        action_button_text, action_enabled, action_menu_item, menu,
    };
    let mut toast = None;
    menu(ui, VIEW_WORKSPACE.label, |ui| {
        let current = world
            .get_resource::<ViewStatus>()
            .map_or_else(|| LEVEL_WORKSPACE.to_owned(), |s| s.workspace.clone());
        // Built in: one registered item each (`editor.menu_list` lists them).
        for workspace in BUILT_IN_WORKSPACES {
            if action_enabled(
                ui,
                current != workspace.id,
                &VIEW_WORKSPACE,
                Some(workspace.title.to_owned()),
            )
            .on_hover_text(workspace.description)
            .clicked()
            {
                queue_ui(ui, world, ViewRequest::SetWorkspace(workspace.id.to_owned()));
                ui.close();
            }
        }
        // The user's workspace layouts (`editor.workspace_set { id: "user:<name>" }`).
        for mut entry in workspace_entries_in(world).into_iter().filter(|e| !e.built_in) {
            if entry.extension.is_some() {
                entry.title.push_str(" (extension)");
            }
            if ui
                .selectable_label(entry.id == current, entry.title.as_str())
                .clicked()
            {
                queue_ui(ui, world, ViewRequest::SetWorkspace(entry.id.clone()));
                ui.close();
            }
        }
    });
    menu(ui, VIEW_NAMED_LAYOUTS.label, |ui| {
        toast = layouts_menu(ui, world, naming)
    });
    if action_menu_item(ui, &VIEW_MAXIMIZE_PANEL).clicked() {
        ViewRequests::push(world, ViewRequest::ToggleMaximize(None));
        ui.close();
    }
    if action_menu_item(ui, &VIEW_FULLSCREEN_VIEWPORT).clicked() {
        ViewRequests::push(world, ViewRequest::ToggleFullscreen);
        ui.close();
    }
    menu(ui, VIEW_PANELS.label, |ui| {
        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
            for panel in crate::editor_layout::BUILT_IN_PANELS {
                if crate::feature_profiles::panel_hidden(world, *panel) {
                    continue;
                }
                // `editor.layout_apply { open }`.
                if action_button_text(ui, &VIEW_PANELS, panel.title()).clicked() {
                    LayoutRequests::push(world, LayoutRequest::Open(*panel));
                    ui.close();
                }
            }
        });
    });
    toast
}

/// The text fields of View ▸ Layouts (kept by the shell between frames).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutNaming {
    /// The name to save the current layout under.
    pub new_name: String,
    /// Save it as a workspace.
    pub as_workspace: bool,
    /// The new name typed for a rename.
    pub rename_to: String,
}

fn layouts_menu(ui: &mut egui::Ui, world: &mut World, naming: &mut LayoutNaming) -> Option<String> {
    let dir = layout_dir_of(world);
    let mut toast = None;
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut naming.new_name)
                .hint_text("Layout name")
                .desired_width(160.0),
        )
        .on_hover_text("Name for the current layout");
        // `editor.layout_save_named { name, workspace }`.
        ui.checkbox(&mut naming.as_workspace, "Workspace");
        if crate::agent_parity::action_enabled(
            ui,
            dir.is_some(),
            &crate::agent_parity::VIEW_NAMED_LAYOUTS,
            Some("Save".to_owned()),
        )
        .clicked()
            && let Some(dir) = &dir
        {
            toast = Some(
                match save_current(world, dir, &naming.new_name, naming.as_workspace) {
                    Ok(saved) => format!("Layout saved as {}", saved.name),
                    Err(err) => format!("Layout not saved: {err}"),
                },
            );
        }
    });
    let Some(dir) = dir else {
        ui.weak("No user config directory: named layouts are not saved");
        return toast;
    };
    ui.separator();
    let layouts = list_named_layouts(&dir);
    if layouts.is_empty() {
        ui.weak("No named layouts yet");
    }
    ui.add(
        egui::TextEdit::singleline(&mut naming.rename_to)
            .hint_text("New name (for Rename)")
            .desired_width(160.0),
    )
    .on_hover_text("Type a new name, then pick Rename on a layout");
    for layout in layouts {
        ui.horizontal(|ui| {
            let label = if layout.workspace {
                format!("{} (workspace)", layout.name)
            } else {
                layout.name.clone()
            };
            // `editor.layout_apply_named { name }`.
            if ui.button(label).on_hover_text("Show this layout").clicked() {
                toast = Some(match apply_named(world, &dir, &layout.name) {
                    Ok(_) => format!("Layout {}", layout.name),
                    Err(err) => format!("Layout not applied: {err}"),
                });
            }
            // `editor.layout_save_named { name, rename_from }`.
            if ui.small_button("Rename").clicked() {
                toast = Some(match rename_named_layout(&dir, &layout.name, &naming.rename_to) {
                    Ok(renamed) => format!("Renamed to {}", renamed.name),
                    Err(err) => format!("Not renamed: {err}"),
                });
            }
            // `editor.layout_delete { name }`.
            if ui.small_button("Delete").clicked() {
                toast = Some(match delete_named_layout(&dir, &layout.name) {
                    Ok(_) => format!("Deleted layout {}", layout.name),
                    Err(err) => format!("Not deleted: {err}"),
                });
            }
        });
    }
    toast
}

/// Saves the shown layout (the published snapshot) as `name`.
///
/// # Errors
///
/// `no_layout` before the first frame, or [`save_named_layout`]'s errors.
pub fn save_current(
    world: &World,
    dir: &Path,
    name: &str,
    workspace: bool,
) -> Result<NamedLayout, WorkspaceError> {
    let doc = world
        .get_resource::<LayoutSnapshot>()
        .map(|s| s.doc.clone())
        .ok_or_else(|| WorkspaceError::new("no_layout", "the editor window has not drawn a layout yet"))?;
    save_named_layout(dir, name, &doc, workspace)
}

/// Reads layout `name`, checks that it builds, and queues it for the next frame.
///
/// # Errors
///
/// [`read_named_layout`]'s errors, or the layout's `layout_*` error.
pub fn apply_named(world: &mut World, dir: &Path, name: &str) -> Result<NamedLayout, WorkspaceError> {
    let (layout, doc) = read_named_layout(dir, name)?;
    let _ = doc.to_dock()?;
    LayoutRequests::push(world, LayoutRequest::Apply(doc));
    Ok(layout)
}

/// The `+` popup of a tab group whose tabs are `tabs`: the group's tabs (pick one to bring it
/// forward; the overflow list of a crowded bar), then every panel to add (hidden ones left out).
pub fn add_popup(ui: &mut egui::Ui, world: &mut World, tabs: &[Panel]) {
    ui.set_min_width(180.0);
    if !tabs.is_empty() {
        ui.weak("Tabs in this group");
        for tab in tabs {
            // `editor.panel_replace { to, near }` with `to` already here brings it forward.
            if ui.button(tab.title()).clicked() {
                queue_ui(ui, world, ViewRequest::Focus(*tab));
                ui.close();
            }
        }
        ui.separator();
    }
    if let Some(tab) = tabs.first() {
        area_menu(ui, world, *tab);
    }
    ui.weak("Add panel");
    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
        for panel in crate::editor_layout::BUILT_IN_PANELS {
            if tabs.contains(panel) || crate::feature_profiles::panel_hidden(world, *panel) {
                continue;
            }
            // `editor.panel_replace { to, near }`.
            if ui.button(panel.title()).clicked() {
                queue_ui(
                    ui,
                    world,
                    ViewRequest::AddTab {
                        panel: *panel,
                        near: tabs.first().copied(),
                    },
                );
                ui.close();
            }
        }
    });
}

/// The add popup's pick: the same request `editor.panel_replace { to, near }` queues.
pub fn pick_add(world: &mut World, panel: Panel, near: Option<Panel>) {
    ViewRequests::push(world, ViewRequest::AddTab { panel, near });
}

/// The context menu of tab `tab`: Replace with ▸ (every other panel) and Maximize.
pub fn tab_context_menu(ui: &mut egui::Ui, world: &mut World, tab: Panel) {
    area_menu(ui, world, tab);
    crate::agent_parity::menu(ui, "Replace with", |ui| {
        egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
            for panel in crate::editor_layout::BUILT_IN_PANELS {
                if *panel == tab || crate::feature_profiles::panel_hidden(world, *panel) {
                    continue;
                }
                // `editor.panel_replace { from, to }`.
                if ui.button(panel.title()).clicked() {
                    queue_ui(
                        ui,
                        world,
                        ViewRequest::Replace {
                            from: tab,
                            to: *panel,
                        },
                    );
                    ui.close();
                }
            }
        });
    });
    // `editor.maximize_panel { panel }`.
    if ui.button("Maximize (Ctrl+Space)").clicked() {
        ViewRequests::push(world, ViewRequest::ToggleMaximize(Some(tab)));
        ui.close();
    }
}

fn area_menu(ui: &mut egui::Ui, world: &mut World, tab: Panel) {
    let groups = world.get_resource::<DockGroups>().cloned().unwrap_or_default();
    let own = groups
        .groups
        .iter()
        .find(|(_, tabs)| tabs.contains(&tab))
        .map(|(key, _)| *key);
    let neighbors = own
        .and_then(|key| groups.neighbors.get(&key))
        .cloned()
        .unwrap_or_default();
    crate::agent_parity::menu(ui, "Join with neighbor", |ui| {
        for into in neighbors {
            if crate::agent_parity::action_button_text(ui, &crate::agent_parity::WORKSPACE_JOIN, into.title())
                .clicked()
            {
                queue_ui(ui, world, ViewRequest::Join { from: tab, into });
                ui.close();
            }
        }
    });
    crate::agent_parity::menu(ui, "Swap with", |ui| {
        for (key, tabs) in &groups.groups {
            if Some(*key) == own {
                continue;
            }
            if let Some(other) = tabs.first().copied()
                && crate::agent_parity::action_button_text(
                    ui,
                    &crate::agent_parity::WORKSPACE_SWAP,
                    other.title(),
                )
                .clicked()
            {
                queue_ui(ui, world, ViewRequest::Swap { from: tab, other });
                ui.close();
            }
        }
    });
}

/// The context menu's Replace with pick: the same request `editor.panel_replace { from, to }`
/// queues.
pub fn pick_replace(world: &mut World, from: Panel, to: Panel) {
    ViewRequests::push(world, ViewRequest::Replace { from, to });
}

fn groups_fingerprint(dock: &DockState<Panel>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    crate::editor_layout::dock_fingerprint(dock).hash(&mut hasher);
    for (path, node) in dock.iter_all_nodes() {
        path.hash(&mut hasher);
        match node {
            egui_dock::Node::Horizontal(split) => (0u8, split.fraction.to_bits()).hash(&mut hasher),
            egui_dock::Node::Vertical(split) => (1u8, split.fraction.to_bits()).hash(&mut hasher),
            _ => {}
        }
    }
    hasher.finish()
}

/// The tab groups' tabs by `(surface, node)`, captured before the dock draws so the add popup can
/// list a group's tabs while the dock is borrowed.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct DockGroups {
    /// Tabs per group.
    pub groups: BTreeMap<(usize, usize), Vec<Panel>>,
    /// Fingerprint of the captured tabs and split topology on every surface.
    pub fingerprint: u64,
    /// Neighboring areas, represented by their first tab.
    pub neighbors: BTreeMap<(usize, usize), Vec<Panel>>,
}

impl DockGroups {
    /// Captures `dock`'s groups.
    #[must_use]
    pub fn of(dock: &DockState<Panel>) -> Self {
        let _span = tracing::debug_span!("editor.workspace_groups").entered();
        Self {
            neighbors: dock
                .iter_leaves()
                .map(|(path, _)| {
                    let neighbors = dock
                        .iter_leaves()
                        .filter(|(other, _)| neighboring(dock, path, *other))
                        .filter_map(|(_, leaf)| leaf.tabs.first().copied())
                        .collect();
                    ((path.surface.0, path.node.0), neighbors)
                })
                .collect(),
            groups: dock
                .iter_leaves()
                .map(|(path, leaf)| ((path.surface.0, path.node.0), leaf.tabs.clone()))
                .collect(),
            fingerprint: groups_fingerprint(dock),
        }
    }

    /// Recaptures `dock` into `world` when it changed (the fingerprint allocates nothing, so an
    /// unchanged dock costs no allocation per frame).
    pub fn refresh(world: &mut World, dock: &DockState<Panel>) {
        let _span = tracing::trace_span!("editor.workspace_groups_refresh").entered();
        let fingerprint = groups_fingerprint(dock);
        if world
            .get_resource::<Self>()
            .is_some_and(|groups| groups.fingerprint == fingerprint)
        {
            return;
        }
        world.insert_resource(Self::of(dock));
        world.insert_resource(AreaSnapshot(dock.clone()));
    }

    /// The tabs of the group at `path`.
    #[must_use]
    pub fn tabs(&self, path: egui_dock::NodePath) -> &[Panel] {
        self.groups
            .get(&(path.surface.0, path.node.0))
            .map_or(&[], Vec::as_slice)
    }
}

/// The width of the tab-overflow button (`▾`) at the right end of every tab bar (ADR-7753 item 4 as
/// amended by ADR-7902). egui_dock 0.21 has no hook for a second tab-bar button, so the room is the
/// tab bar's right inner margin ([`reserve_overflow_button`]) and the shell draws the button there
/// after the dock ([`tab_overflow_buttons`]).
pub const OVERFLOW_BUTTON_WIDTH: f32 = 24.0;

/// The width of egui_dock's `+` button (its private `TAB_ADD_BUTTON_SIZE` in 0.21), which sits just
/// left of the overflow button and is not part of the tabs' visible strip.
const ADD_BUTTON_WIDTH: f32 = 24.0;

/// Reserves the overflow button's room at the right end of `style`'s tab bars.
pub fn reserve_overflow_button(style: &mut egui_dock::Style) {
    const WIDTH: i8 = OVERFLOW_BUTTON_WIDTH as i8;
    style.tab_bar.inner_margin.right = style.tab_bar.inner_margin.right.saturating_add(WIDTH);
}

/// Reserves room for the accessible add-panel and overflow buttons.
pub fn reserve_tab_buttons(style: &mut egui_dock::Style) {
    reserve_overflow_button(style);
    style.tab_bar.inner_margin.right = style
        .tab_bar
        .inner_margin
        .right
        .saturating_add(ADD_BUTTON_WIDTH as i8);
}

/// The tab buttons drawn this frame and each tab group's hidden tabs, for the overflow button and
/// `editor.tab_overflow`.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct TabStrips {
    /// The egui frame the tab rects belong to.
    pub frame: u64,
    /// Each tab button's rect drawn in [`Self::frame`] (egui_dock's `on_tab_button`).
    pub rects: Vec<(Panel, egui::Rect)>,
    /// The hidden tabs of each tab group drawn last, by `(surface, node)`, in tab order.
    pub hidden: BTreeMap<(usize, usize), Vec<Panel>>,
    /// The tab bar height the dock drew with.
    pub bar_height: f32,
}

impl TabStrips {
    /// Records `tab`'s button rect for egui frame `frame` (egui's second pass of a frame replaces the
    /// first pass's rect).
    pub fn note(&mut self, tab: Panel, rect: egui::Rect, frame: u64) {
        if frame != self.frame {
            self.frame = frame;
            self.rects.clear();
        }
        match self.rects.iter_mut().find(|(t, _)| *t == tab) {
            Some(entry) => entry.1 = rect,
            None => self.rects.push((tab, rect)),
        }
    }

    /// The rect drawn for `tab` this frame.
    #[must_use]
    pub fn rect(&self, tab: Panel) -> Option<egui::Rect> {
        self.rects.iter().find(|(t, _)| *t == tab).map(|(_, r)| *r)
    }
}

/// Records a tab button's rect (called from the shell's `on_tab_button`).
pub fn note_tab_rect(world: &mut World, tab: Panel, response: &egui::Response) {
    let frame = response.ctx.cumulative_frame_nr();
    world
        .get_resource_or_insert_with(TabStrips::default)
        .note(tab, response.rect, frame);
}

/// The tabs of `tabs` that are not wholly inside the visible strip `visible` (an x range) of their tab
/// bar: scrolled out or clipped. A tab with no rect this frame (not drawn) counts as hidden.
#[must_use]
pub fn hidden_tabs(tabs: &[Panel], strips: &TabStrips, visible: egui::Rangef) -> Vec<Panel> {
    // Half a point of slack: a tab that ends exactly on the edge is shown.
    const SLACK: f32 = 0.5;
    tabs.iter()
        .copied()
        .filter(|tab| {
            strips
                .rect(*tab)
                .is_none_or(|rect| rect.left() < visible.min - SLACK || rect.right() > visible.max + SLACK)
        })
        .collect()
}

/// egui_dock's tab-bar scroll offset (`LeafNode::scroll`, zero or negative: the tabs move left by
/// its size) that brings `tab` (its rect drawn at offset `scroll`) wholly into `visible`, or the
/// nearest it can when the tab is wider than the strip (its left edge shows).
#[must_use]
pub fn scroll_to_reveal(scroll: f32, tab: egui::Rect, visible: egui::Rangef) -> f32 {
    let mut out = scroll;
    if tab.right() > visible.max {
        out -= tab.right() - visible.max;
    }
    let left = tab.left() + (out - scroll);
    if left < visible.min {
        out += visible.min - left;
    }
    out.min(0.0)
}

/// The tab bar rect of a leaf whose whole rect is `leaf_rect`.
fn tab_bar_rect(leaf_rect: egui::Rect, bar_height: f32) -> egui::Rect {
    egui::Rect::from_min_size(leaf_rect.min, egui::vec2(leaf_rect.width(), bar_height))
}

/// The x range in which a tab bar `bar` shows tabs: the bar less the `+` button and the overflow
/// button at its right end.
#[must_use]
pub fn visible_tab_strip(bar: egui::Rect) -> egui::Rangef {
    egui::Rangef::new(
        bar.left(),
        (bar.right() - OVERFLOW_BUTTON_WIDTH - ADD_BUTTON_WIDTH).max(bar.left()),
    )
}

/// After the dock drew: works out every tab group's hidden tabs ([`TabStrips::hidden`]) and draws
/// the overflow button (`▾`) at the right end of each tab bar, whose menu lists the hidden tabs (pick
/// one to bring it forward, the request `editor.panel_replace { to, near }` queues). A floating
/// window's button goes on that window's layer.
pub fn tab_overflow_buttons(ui: &mut egui::Ui, world: &mut World, dock: &DockState<Panel>, bar_height: f32) {
    tab_buttons(ui, world, dock, bar_height, None);
}

/// Draws the same named, keyboard-operable tab controls in an OS window whose dock is temporarily
/// represented as the main surface. `surface` is its address in the shell's complete dock.
pub fn window_tab_buttons(
    ui: &mut egui::Ui,
    world: &mut World,
    dock: &DockState<Panel>,
    bar_height: f32,
    surface: egui_dock::SurfaceIndex,
) {
    tab_buttons(ui, world, dock, bar_height, Some(surface));
}

fn tab_buttons(
    ui: &mut egui::Ui,
    world: &mut World,
    dock: &DockState<Panel>,
    bar_height: f32,
    window_surface: Option<egui_dock::SurfaceIndex>,
) {
    let mut strips = world.remove_resource::<TabStrips>().unwrap_or_default();
    if let Some(surface) = window_surface {
        strips.hidden.retain(|(s, _), _| *s != surface.0);
    } else {
        strips.hidden.clear();
    }
    strips.bar_height = bar_height;
    for (path, leaf) in dock.iter_leaves() {
        if leaf.tabs.is_empty() || leaf.rect.width() < OVERFLOW_BUTTON_WIDTH + ADD_BUTTON_WIDTH {
            continue;
        }
        let address = if path.surface.is_main() {
            window_surface.unwrap_or(path.surface)
        } else {
            path.surface
        };
        let bar = tab_bar_rect(leaf.rect, bar_height);
        let hidden = hidden_tabs(&leaf.tabs, &strips, visible_tab_strip(bar));
        strips.hidden.insert((address.0, path.node.0), hidden.clone());
        let button_rect = egui::Rect::from_min_max(
            egui::pos2(bar.right() - OVERFLOW_BUTTON_WIDTH, bar.top()),
            bar.right_bottom(),
        );
        let layer = if path.surface.0 == 0 {
            ui.layer_id()
        } else {
            match ui.ctx().layer_id_at(button_rect.center()) {
                Some(layer) if layer.order == egui::Order::Middle => layer,
                _ => continue,
            }
        };
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .layer_id(layer)
                .max_rect(button_rect)
                .id_salt(("tab_overflow", address.0, path.node.0))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        overflow_menu(&mut child, world, &hidden);
        let add_rect = button_rect.translate(egui::vec2(-ADD_BUTTON_WIDTH, 0.0));
        let mut add = ui.new_child(
            egui::UiBuilder::new()
                .layer_id(layer)
                .max_rect(add_rect)
                .id_salt(("tab_add", address.0, path.node.0))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        let response = add.menu_button("+", |ui| add_popup(ui, world, &leaf.tabs));
        crate::a11y_audit::name(response.response, egui::WidgetType::Button, "Add panel");
    }
    world.insert_resource(strips);
}

/// The overflow button and its menu: `hidden` (the group's hidden tabs), or a note that none is.
fn overflow_menu(ui: &mut egui::Ui, world: &mut World, hidden: &[Panel]) {
    // `editor.tab_overflow` lists the same tabs.
    let response = ui.menu_button("▾", |ui| {
        ui.set_min_width(160.0);
        if hidden.is_empty() {
            ui.weak("Every tab is shown");
        }
        for tab in hidden {
            // `editor.panel_replace { to, near }` with `to` already here brings it forward.
            if ui.button(tab.title()).clicked() {
                queue_ui(ui, world, ViewRequest::Focus(*tab));
                ui.close();
            }
        }
    });
    crate::a11y_audit::name(response.response, egui::WidgetType::Button, "Hidden tabs");
}

// ---------------------------------------------------------------------------------------------
// Agent methods

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

fn bad(code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

fn dir_or_err(world: &World) -> Result<PathBuf, RpcError> {
    layout_dir_of(world).ok_or_else(|| {
        bad(
            "no_user_config",
            "this editor keeps no per-user files",
            "run the editor binary",
        )
    })
}

fn str_param<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

fn panel_param(params: &Value, key: &str) -> Result<Option<Panel>, RpcError> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => {
            let id = v.as_str().unwrap_or_default();
            Panel::from_id(id).map(Some).ok_or_else(|| {
                bad(
                    "unknown_panel",
                    format!("no panel has the id {id:?}"),
                    "ids such as viewport, content, anim_graph",
                )
            })
        }
    }
}

fn layout_json(layout: &NamedLayout) -> Value {
    json!({ "name": layout.name, "workspace": layout.workspace, "path": layout.path.display().to_string() })
}

fn view_json(world: &World) -> Value {
    let status = world.get_resource::<ViewStatus>().cloned().unwrap_or_default();
    json!({
        "workspace": status.workspace,
        "mode": status.mode.id(),
        "maximized": status.maximized,
        "fullscreen": status.fullscreen,
        "parked": status.parked,
        "pending_requests": world.get_resource::<ViewRequests>().map_or(0, |r| r.pending.len()),
    })
}

/// `editor.layout_list`.
fn layout_list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let dir = dir_or_err(world)?;
    let layouts: Vec<Value> = list_named_layouts(&dir).iter().map(layout_json).collect();
    Ok(json!({ "layouts": layouts, "dir": dir.display().to_string() }))
}

/// `editor.layout_save_named { name, workspace?, rename_from? }`.
fn layout_save_named(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let dir = dir_or_err(world)?;
    let name = str_param(params, "name").ok_or_else(|| bad("bad_layout_name", "`name` is required", ""))?;
    let saved = match str_param(params, "rename_from") {
        Some(from) => rename_named_layout(&dir, from, name),
        None => {
            let workspace = params.get("workspace").and_then(Value::as_bool).unwrap_or(false);
            save_current(world, &dir, name, workspace)
        }
    }
    .map_err(|e| e.to_rpc())?;
    Ok(
        json!({ "saved": layout_json(&saved), "layouts": list_named_layouts(&dir).iter().map(layout_json).collect::<Vec<_>>() }),
    )
}

/// `editor.layout_apply_named { name }`.
fn layout_apply_named(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let dir = dir_or_err(world)?;
    let name = str_param(params, "name").ok_or_else(|| bad("bad_layout_name", "`name` is required", ""))?;
    let layout = apply_named(world, &dir, name).map_err(|e| e.to_rpc())?;
    Ok(json!({ "queued": layout_json(&layout) }))
}

/// `editor.layout_delete { name }`.
fn layout_delete(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let dir = dir_or_err(world)?;
    let name = str_param(params, "name").ok_or_else(|| bad("bad_layout_name", "`name` is required", ""))?;
    let gone = delete_named_layout(&dir, name).map_err(|e| e.to_rpc())?;
    Ok(
        json!({ "deleted": layout_json(&gone), "layouts": list_named_layouts(&dir).iter().map(layout_json).collect::<Vec<_>>() }),
    )
}

/// `editor.workspace_list`.
fn workspace_list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let workspaces: Vec<Value> = workspace_entries_in(world)
        .into_iter()
        .map(|entry| {
            let built_in = built_in_workspace(&entry.id);
            json!({
                "id": entry.id,
                "title": entry.title,
                "built_in": entry.built_in,
                "mode": entry.mode.id(),
                "extension": entry.extension,
                "description": built_in.map(|w| w.description),
                "panels": built_in.map(|w| w.panels().iter().map(|p| p.id()).collect::<Vec<_>>()),
                "opens_profiler": built_in.is_some_and(|w| w.opens_profiler),
            })
        })
        .collect();
    let mut out = view_json(world);
    out["workspaces"] = json!(workspaces);
    Ok(out)
}

/// `editor.workspace_set { id }`.
fn workspace_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = str_param(params, "id")
        .ok_or_else(|| bad("unknown_workspace", "`id` is required", "editor.workspace_list"))?;
    let known = id
        .strip_prefix("extension:")
        .is_some_and(|id| crate::extension::context::live_workspaces(world).any(|(_, w)| w.id == id))
        || built_in_workspace(id).is_some()
        || id.strip_prefix(USER_WORKSPACE_PREFIX).is_some_and(|name| {
            layout_dir_of(world)
                .and_then(|dir| find_named_layout(&dir, name))
                .is_some_and(|l| l.workspace)
        });
    if !known {
        return Err(unknown_workspace(id).to_rpc());
    }
    let request = match params.get("mode") {
        None => ViewRequest::SetWorkspace(id.to_owned()),
        Some(value) => {
            let mode = value.as_str().and_then(EditorMode::from_id).ok_or_else(|| {
                bad(
                    "workspace_mode",
                    "mode must be world, terrain, tiles2d, animation or ui_design",
                    "editor.workspace_list",
                )
            })?;
            ViewRequest::SetWorkspaceMode {
                id: id.to_owned(),
                mode,
            }
        }
    };
    let mode = match &request {
        ViewRequest::SetWorkspaceMode { mode, .. } => *mode,
        _ => workspace_mode(world, id),
    };
    ViewRequests::push(world, request);
    let mut out = view_json(world);
    out["queued"] = json!({ "workspace": id, "mode": mode.id() });
    Ok(out)
}

/// `editor.maximize_panel { panel?, fullscreen? }`.
fn maximize_panel(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let panel = panel_param(params, "panel")?;
    let fullscreen = params.get("fullscreen").and_then(Value::as_bool).unwrap_or(false);
    let open = world
        .get_resource::<LayoutSnapshot>()
        .is_none_or(|s| panel.is_none_or(|p| s.doc.panel_ids().contains(&p.id())));
    let status = world.get_resource::<ViewStatus>().cloned().unwrap_or_default();
    if !fullscreen && status.maximized.is_none() && !open {
        return Err(panel.map_or_else(
            || bad("panel_not_open", "no tab to maximize", ""),
            |p| panel_not_open(p).to_rpc(),
        ));
    }
    let request = if fullscreen {
        ViewRequest::ToggleFullscreen
    } else {
        ViewRequest::ToggleMaximize(panel)
    };
    ViewRequests::push(world, request);
    let mut out = view_json(world);
    out["queued"] = json!(if fullscreen { "fullscreen" } else { "maximize" });
    Ok(out)
}

/// `editor.panel_replace { to, from? | near? }`.
fn panel_replace(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let to = panel_param(params, "to")?
        .ok_or_else(|| bad("bad_pane_request", "`to` is required", "a panel id"))?;
    let from = panel_param(params, "from")?;
    let near = panel_param(params, "near")?;
    if from.is_some() && near.is_some() {
        return Err(bad(
            "bad_pane_request",
            "pass `from` (replace) or `near` (add), not both",
            "",
        ));
    }
    let open: Vec<String> = world
        .get_resource::<LayoutSnapshot>()
        .map(|s| s.doc.panel_ids().iter().map(|id| (*id).to_owned()).collect())
        .unwrap_or_default();
    let check_open = |panel: Panel| -> Result<(), RpcError> {
        if world.contains_resource::<LayoutSnapshot>() && !open.iter().any(|id| id == panel.id()) {
            return Err(panel_not_open(panel).to_rpc());
        }
        Ok(())
    };
    let queued = match from {
        Some(from) => {
            check_open(from)?;
            pick_replace(world, from, to);
            json!({ "replace": { "from": from.id(), "to": to.id() } })
        }
        None => {
            if let Some(near) = near {
                check_open(near)?;
            }
            pick_add(world, to, near);
            json!({ "add": { "panel": to.id(), "near": near.map(Panel::id) } })
        }
    };
    let mut out = view_json(world);
    out["queued"] = queued;
    Ok(out)
}

fn snapshot_dock(world: &World) -> Result<DockState<Panel>, RpcError> {
    if let Some(snapshot) = world.get_resource::<AreaSnapshot>() {
        return Ok(snapshot.0.clone());
    }
    world
        .get_resource::<LayoutSnapshot>()
        .ok_or_else(|| {
            bad(
                "no_layout",
                "the editor has not published a layout",
                "draw a frame first",
            )
        })?
        .doc
        .to_dock()
        .map(|(dock, _)| dock)
        .map_err(|e| WorkspaceError::from(e).to_rpc())
}

fn area_param(dock: &DockState<Panel>, params: &Value, key: &str) -> Result<Panel, RpcError> {
    let id = str_param(params, key).ok_or_else(|| {
        bad(
            "bad_pane_request",
            format!("`{key}` must be a panel id or area address"),
            "editor.tab_overflow",
        )
    })?;
    if let Some(address) = id.strip_prefix("area:") {
        let path = address
            .split_once(':')
            .and_then(|(s, n)| {
                Some(egui_dock::NodePath::new(
                    egui_dock::SurfaceIndex(s.parse().ok()?),
                    NodeIndex(n.parse().ok()?),
                ))
            })
            .ok_or_else(|| bad_area().to_rpc())?;
        return dock
            .leaf(path)
            .ok()
            .and_then(|leaf| leaf.tabs.first().copied())
            .ok_or_else(|| bad_area().to_rpc());
    }
    let panel = Panel::from_id(id).ok_or_else(|| {
        bad(
            "unknown_panel",
            format!("unknown panel {id:?}"),
            "editor.tab_overflow",
        )
    })?;
    if dock.find_tab(&panel).is_none() {
        return Err(panel_not_open(panel).to_rpc());
    }
    Ok(panel)
}

fn layout_join(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut candidate = snapshot_dock(world)?;
    let from = area_param(&candidate, params, "from")?;
    let into = area_param(&candidate, params, "into")?;
    join_areas(&mut candidate, from, into).map_err(|e| e.to_rpc())?;
    ViewRequests::push(world, ViewRequest::Join { from, into });
    Ok(json!({"queued": {"from": params["from"], "into": params["into"]}}))
}

fn layout_swap(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut candidate = snapshot_dock(world)?;
    let from = area_param(&candidate, params, "from")?;
    let other = area_param(&candidate, params, "with")?;
    swap_areas(&mut candidate, from, other).map_err(|e| e.to_rpc())?;
    ViewRequests::push(world, ViewRequest::Swap { from, other });
    Ok(json!({"queued": {"from": params["from"], "with": params["with"]}}))
}

/// `editor.tab_overflow`: each tab group's tabs and the ones its bar hides (the overflow button's
/// list, as of the last drawn frame).
fn tab_overflow(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let groups = world.get_resource::<DockGroups>().cloned().unwrap_or_default();
    let strips = world.get_resource::<TabStrips>().cloned().unwrap_or_default();
    let ids = |tabs: &[Panel]| tabs.iter().map(|t| t.id()).collect::<Vec<_>>();
    let out: Vec<Value> = groups
        .groups
        .iter()
        .map(|((surface, node), tabs)| {
            let hidden = strips
                .hidden
                .get(&(*surface, *node))
                .map_or(&[][..], Vec::as_slice);
            json!({
                "surface": surface,
                "node": node,
                "tabs": ids(tabs),
                "hidden": ids(hidden),
            })
        })
        .collect();
    Ok(json!({ "groups": out, "drawn": !strips.rects.is_empty() }))
}

/// This module's agent methods, installed by `agent_ext::all_methods`.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    const NAME: ParamSchema = ParamSchema {
        name: "name",
        kind: "string",
        required: true,
        description: "The layout name: 1-64 letters, digits, spaces, - and _ (compared without case).",
    };
    vec![
        (
            MethodSchema {
                name: "editor.layout_list",
                tool: "layout_list",
                description: "The user's named layouts (each a file in the user config's layouts folder), with whether each is a workspace.",
                params: &[],
                result: "{ layouts: [{ name, workspace, path }], dir }",
                mutating: false,
                permission: Permission::Read,
            },
            layout_list as Handler,
        ),
        (
            MethodSchema {
                name: "editor.layout_save_named",
                tool: "layout_save_named",
                description: "Save the shown dock layout under a name (replacing a layout of that name), optionally as a workspace; or rename a saved layout with rename_from.",
                params: &[
                    NAME,
                    ParamSchema {
                        name: "workspace",
                        kind: "boolean",
                        required: false,
                        description: "Save it as a user workspace (listed by editor.workspace_list as user:<name>).",
                    },
                    ParamSchema {
                        name: "rename_from",
                        kind: "string",
                        required: false,
                        description: "Rename this saved layout to `name` instead of saving the shown one.",
                    },
                ],
                result: "{ saved: { name, workspace, path }, layouts }",
                mutating: true,
                permission: Permission::EditScene,
            },
            layout_save_named as Handler,
        ),
        (
            MethodSchema {
                name: "editor.layout_apply_named",
                tool: "layout_apply_named",
                description: "Show a named layout on the next frame (validated now).",
                params: &[NAME],
                result: "{ queued: { name, workspace, path } }",
                mutating: true,
                permission: Permission::EditScene,
            },
            layout_apply_named as Handler,
        ),
        (
            MethodSchema {
                name: "editor.layout_delete",
                tool: "layout_delete",
                description: "Delete a named layout file.",
                params: &[NAME],
                result: "{ deleted: { name, workspace, path }, layouts }",
                mutating: true,
                permission: Permission::EditScene,
            },
            layout_delete as Handler,
        ),
        (
            MethodSchema {
                name: "editor.workspace_list",
                tool: "workspace_list",
                description: "The built-in workspaces, user:<name> saved workspaces and live extension:<id> workspaces, with effective tool modes, extension owners and the current view state.",
                params: &[],
                result: "{ workspace, mode, maximized, fullscreen, parked, pending_requests, workspaces: [{ id, title, built_in, mode, extension, description, panels, opens_profiler }] }",
                mutating: false,
                permission: Permission::Read,
            },
            workspace_list as Handler,
        ),
        (
            MethodSchema {
                name: "editor.workspace_set",
                tool: "workspace_set",
                description: "Switch workspace on the next frame. The dock left is kept for this session, so switching back restores it exactly; open documents stay open.",
                params: &[
                    ParamSchema {
                        name: "id",
                        kind: "string",
                        required: true,
                        description: "A workspace id from editor.workspace_list.",
                    },
                    ParamSchema {
                        name: "mode",
                        kind: "string",
                        required: false,
                        description: "Session-local mode override: world, terrain, tiles2d, animation or ui_design.",
                    },
                ],
                result: "the editor.workspace_list view fields plus queued",
                mutating: true,
                permission: Permission::EditScene,
            },
            workspace_set as Handler,
        ),
        (
            MethodSchema {
                name: "editor.maximize_panel",
                tool: "maximize_panel",
                description: "Toggle on the next frame: maximize a tab to the whole dock area (or restore the exact previous dock when one is maximized), or with fullscreen the viewport alone with the editor chrome hidden.",
                params: &[
                    ParamSchema {
                        name: "panel",
                        kind: "string",
                        required: false,
                        description: "The panel id to maximize; omitted takes the focused tab.",
                    },
                    ParamSchema {
                        name: "fullscreen",
                        kind: "boolean",
                        required: false,
                        description: "Toggle the full-screen viewport instead.",
                    },
                ],
                result: "the editor.workspace_list view fields plus queued",
                mutating: true,
                permission: Permission::EditScene,
            },
            maximize_panel as Handler,
        ),
        (
            MethodSchema {
                name: "editor.panel_replace",
                tool: "panel_replace",
                description: "Change a pane's panel on the next frame: with from, put `to` in place of that tab; otherwise add `to` to the tab group of `near` (or the focused group). A panel open elsewhere moves.",
                params: &[
                    ParamSchema {
                        name: "to",
                        kind: "string",
                        required: true,
                        description: "The panel id to show.",
                    },
                    ParamSchema {
                        name: "from",
                        kind: "string",
                        required: false,
                        description: "The tab to replace.",
                    },
                    ParamSchema {
                        name: "near",
                        kind: "string",
                        required: false,
                        description: "A tab of the group to add to.",
                    },
                ],
                result: "the editor.workspace_list view fields plus queued",
                mutating: true,
                permission: Permission::EditScene,
            },
            panel_replace as Handler,
        ),
        (
            MethodSchema {
                name: "editor.layout_join",
                tool: "layout_join",
                description: "Merge neighboring dock areas into the destination tab stack, preserving its active tab. Atomic; validates again when applied.",
                params: &[
                    ParamSchema {
                        name: "from",
                        kind: "string",
                        required: true,
                        description: "A source panel id or area:<surface>:<node> address from editor.tab_overflow.",
                    },
                    ParamSchema {
                        name: "into",
                        kind: "string",
                        required: true,
                        description: "A neighboring destination panel id or area address.",
                    },
                ],
                result: "{ queued: { from, into } }",
                mutating: true,
                permission: Permission::EditScene,
            },
            layout_join as Handler,
        ),
        (
            MethodSchema {
                name: "editor.layout_swap",
                tool: "layout_swap",
                description: "Exchange two dock areas' complete tab stacks and active tabs, preserving geometry. Atomic; validates again when applied.",
                params: &[
                    ParamSchema {
                        name: "from",
                        kind: "string",
                        required: true,
                        description: "A panel id or area:<surface>:<node> address from editor.tab_overflow.",
                    },
                    ParamSchema {
                        name: "with",
                        kind: "string",
                        required: true,
                        description: "A panel id or area address in a different area.",
                    },
                ],
                result: "{ queued: { from, with } }",
                mutating: true,
                permission: Permission::EditScene,
            },
            layout_swap as Handler,
        ),
        (
            MethodSchema {
                name: "editor.tab_overflow",
                tool: "tab_overflow",
                description: "Each tab group's tabs and the ones its crowded tab bar hides (the list of the tab bar's overflow button, as of the last drawn frame). Bring one forward with editor.panel_replace { to, near }.",
                params: &[],
                result: "groups: [{ surface, node, tabs, hidden }], drawn",
                mutating: false,
                permission: Permission::Read,
            },
            tab_overflow as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::editor_layout::{LayoutDoc, dock_fingerprint};

    fn dir(label: &str) -> PathBuf {
        regia_test_utils::unique_temp_dir(&format!("workspaces-{label}"))
    }

    /// ADR-7902: the overflow list is the tabs not wholly inside the strip left of the `+` and `▾`
    /// buttons; a tab with no rect this frame counts as hidden.
    #[test]
    fn hidden_tabs_are_the_ones_outside_the_visible_strip() {
        let bar = egui::Rect::from_min_size(egui::pos2(100.0, 0.0), egui::vec2(300.0, 24.0));
        let visible = visible_tab_strip(bar);
        assert_eq!((visible.min, visible.max), (100.0, 352.0));
        let mut strips = TabStrips::default();
        let tab = |x: f32| egui::Rect::from_min_size(egui::pos2(x, 0.0), egui::vec2(80.0, 24.0));
        strips.note(Panel::OutputLog, tab(100.0), 7);
        strips.note(Panel::Layers, tab(180.0), 7);
        strips.note(Panel::Problems, tab(260.0), 7); // ends at 340: shown
        strips.note(Panel::Tests, tab(340.0), 7); // ends at 420: clipped
        let tabs = [
            Panel::OutputLog,
            Panel::Layers,
            Panel::Problems,
            Panel::Tests,
            Panel::Theme,
        ];
        assert_eq!(
            hidden_tabs(&tabs, &strips, visible),
            vec![Panel::Tests, Panel::Theme]
        );
        // Scrolled left by 100: the first tab is out on the left.
        strips.note(Panel::OutputLog, tab(0.0), 7);
        assert_eq!(
            hidden_tabs(&tabs[..1], &strips, visible),
            vec![Panel::OutputLog],
            "a tab scrolled out on the left is hidden too"
        );
        // A new frame forgets the old rects; egui's second pass of a frame replaces, never adds.
        strips.note(Panel::Layers, tab(180.0), 7);
        assert_eq!(strips.rects.len(), 4);
        strips.note(Panel::Layers, tab(180.0), 8);
        assert_eq!(strips.rects.len(), 1);
    }

    #[test]
    fn a_picked_tab_scrolls_wholly_into_the_strip() {
        let visible = egui::Rangef::new(0.0, 200.0);
        let tab = |x: f32, w: f32| egui::Rect::from_min_size(egui::pos2(x, 0.0), egui::vec2(w, 24.0));
        // Past the right end: the bar scrolls left until its right edge shows.
        assert_eq!(scroll_to_reveal(0.0, tab(250.0, 60.0), visible), -110.0);
        // Already shown: no change.
        assert_eq!(scroll_to_reveal(-40.0, tab(20.0, 60.0), visible), -40.0);
        // Out on the left: scrolls right, never past zero.
        assert_eq!(scroll_to_reveal(-100.0, tab(-60.0, 50.0), visible), -40.0);
        assert_eq!(scroll_to_reveal(-10.0, tab(-60.0, 50.0), visible), 0.0);
        // Wider than the strip: its left edge shows.
        assert_eq!(scroll_to_reveal(0.0, tab(300.0, 400.0), visible), -300.0);
    }

    #[test]
    fn the_tab_bar_reserves_the_overflow_button() {
        let mut style = egui_dock::Style::from_egui(&egui::Style::default());
        let before = style.tab_bar.inner_margin.right;
        reserve_overflow_button(&mut style);
        assert_eq!(
            f32::from(style.tab_bar.inner_margin.right - before),
            OVERFLOW_BUTTON_WIDTH
        );
    }

    #[test]
    fn names_are_checked_and_never_hold_the_suffix() {
        assert_eq!(check_layout_name("  My layout-2_b ").unwrap(), "My layout-2_b");
        let long = "x".repeat(65);
        for bad in ["", "   ", "a/b", "a.b", "..", long.as_str(), "é"] {
            assert_eq!(
                check_layout_name(bad).unwrap_err().code,
                "bad_layout_name",
                "{bad:?}"
            );
        }
        assert_eq!(
            parse_file_name("a.workspace.editor_layout.ron"),
            Some(("a".into(), true))
        );
        assert_eq!(parse_file_name("a.editor_layout.ron"), Some(("a".into(), false)));
        assert_eq!(parse_file_name("a.b.editor_layout.ron"), None);
        assert_eq!(parse_file_name("notes.txt"), None);
    }

    #[test]
    fn save_list_rename_delete_named_layouts() {
        let dir = dir("files");
        let doc = LayoutDoc::from_dock(&crate::editor_layout::default_dock());
        assert!(list_named_layouts(&dir).is_empty());
        save_named_layout(&dir, "Wide", &doc, false).unwrap();
        save_named_layout(&dir, "anim", &doc, true).unwrap();
        let names: Vec<(String, bool)> = list_named_layouts(&dir)
            .into_iter()
            .map(|l| (l.name, l.workspace))
            .collect();
        assert_eq!(names, vec![("anim".into(), true), ("Wide".into(), false)]);
        // The same name with another case and flag replaces it (one file per name).
        save_named_layout(&dir, "WIDE", &doc, true).unwrap();
        let all = list_named_layouts(&dir);
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|l| l.name == "WIDE" && l.workspace));
        assert_eq!(
            rename_named_layout(&dir, "anim", "wide").unwrap_err().code,
            "layout_exists"
        );
        let renamed = rename_named_layout(&dir, "anim", "Animating").unwrap();
        assert!(renamed.workspace);
        assert_eq!(read_named_layout(&dir, "animating").unwrap().1, doc);
        assert_eq!(
            delete_named_layout(&dir, "nope").unwrap_err().code,
            "layout_not_found"
        );
        delete_named_layout(&dir, "Animating").unwrap();
        assert_eq!(list_named_layouts(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_built_in_workspace_dock_holds_its_panels_once() {
        for workspace in BUILT_IN_WORKSPACES {
            let dock = workspace.dock();
            let tabs: Vec<Panel> = dock.iter_all_tabs().map(|(_, t)| *t).collect();
            for panel in workspace.panels() {
                assert_eq!(
                    tabs.iter().filter(|t| **t == panel).count(),
                    1,
                    "{} {}",
                    workspace.id,
                    panel.id()
                );
            }
            assert_eq!(tabs.len(), workspace.panels().len(), "{}", workspace.id);
            // The tree survives the layout file.
            let doc = LayoutDoc::from_dock(&dock);
            let (back, _) = doc.to_dock().unwrap();
            assert_eq!(LayoutDoc::from_dock(&back).root, doc.root, "{}", workspace.id);
        }
        assert_eq!(
            BUILT_IN_WORKSPACES.iter().map(|w| w.id).collect::<Vec<_>>(),
            [
                "level",
                "animation",
                "materials",
                "ui",
                "world",
                "cinematics",
                "debug",
                "terrain",
                "tiles2d"
            ]
        );
    }

    /// Review sprint-v06-rev-late-k #11: the toolbar label needs no folder listing and matches
    /// the listed entries' titles.
    #[test]
    fn the_switcher_label_comes_from_the_id() {
        for entry in workspace_entries(None) {
            assert_eq!(workspace_label(&entry.id), entry.title);
        }
        assert_eq!(workspace_label(&format!("{USER_WORKSPACE_PREFIX}Mine")), "Mine");
        assert_eq!(workspace_label("odd"), "odd");
    }

    /// Review sprint-v06-rev-late-k #9: View ▸ Maximize, the palette and the agent take the
    /// focused tab (the pointer rests over some dock group after a menu click); only Ctrl+Space
    /// takes the tab group under the pointer.
    #[test]
    fn maximize_takes_the_focused_tab_and_only_the_key_takes_the_hovered_one() {
        let mut world = World::new();
        let mut views = DockViews::default();
        let mut dock = crate::editor_layout::default_dock();
        // Without a focused group the request falls back to the viewport.
        let focused = focused_tab(&dock).unwrap_or(Panel::Viewport);
        let other = dock
            .iter_all_tabs()
            .map(|(_, tab)| *tab)
            .find(|tab| *tab != focused)
            .expect("a second tab");
        // The menu / palette / agent request: the focused tab, whatever is under the pointer.
        ViewRequests::push(&mut world, ViewRequest::ToggleMaximize(None));
        views.apply_requests(&mut dock, &mut world);
        assert_eq!(views.maximized(), Some(focused));
        ViewRequests::push(&mut world, ViewRequest::ToggleMaximize(None));
        views.apply_requests(&mut dock, &mut world);
        assert_eq!(views.maximized(), None, "the second toggle restores");
        // The key path names the hovered group's tab.
        let ctx = egui::Context::default();
        let mut raw = egui::RawInput::default();
        raw.events.push(egui::Event::Key {
            key: egui::Key::Space,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        let mut output = ctx.run_ui(raw, |ui| handle_view_keys(ui.ctx(), &mut world, Some(other)));
        output.textures_delta.clear();
        assert_eq!(
            world.resource::<ViewRequests>().pending,
            [ViewRequest::ToggleMaximize(Some(other))]
        );
        views.apply_requests(&mut dock, &mut world);
        assert_eq!(views.maximized(), Some(other));
    }

    #[test]
    fn maximize_and_restore_leave_the_dock_unchanged() {
        let mut world = World::new();
        let mut views = DockViews::default();
        let mut dock = crate::editor_layout::default_dock();
        let before = (LayoutDoc::from_dock(&dock), dock_fingerprint(&dock));
        views
            .apply(
                &mut dock,
                &mut world,
                ViewRequest::ToggleMaximize(Some(Panel::Content)),
            )
            .unwrap();
        assert_eq!(views.maximized(), Some(Panel::Content));
        assert_eq!(dock.iter_all_tabs().count(), 1);
        assert_eq!(LayoutDoc::from_dock(views.base_dock(&dock)), before.0);
        views
            .apply(&mut dock, &mut world, ViewRequest::ToggleMaximize(None))
            .unwrap();
        assert_eq!((LayoutDoc::from_dock(&dock), dock_fingerprint(&dock)), before);
        // Full screen: the chrome hides and Escape's request leaves it.
        views
            .apply(&mut dock, &mut world, ViewRequest::ToggleFullscreen)
            .unwrap();
        assert!(views.chrome_hidden());
        views
            .apply(&mut dock, &mut world, ViewRequest::LeaveFullscreen)
            .unwrap();
        assert!(!views.chrome_hidden());
        assert_eq!((LayoutDoc::from_dock(&dock), dock_fingerprint(&dock)), before);
        // A closed panel cannot be maximized.
        assert_eq!(
            views
                .apply(
                    &mut dock,
                    &mut world,
                    ViewRequest::ToggleMaximize(Some(Panel::Preferences)),
                )
                .unwrap_err()
                .code,
            "panel_not_open"
        );
    }

    #[test]
    fn built_in_chords_match_the_palette_shortcuts() {
        use crate::command_palette::{KeyChord, find};
        assert_eq!(
            KeyChord::parse(find(MAXIMIZE_COMMAND).unwrap().shortcut),
            Some(MAXIMIZE_CHORD)
        );
        assert_eq!(
            KeyChord::parse(find(FULLSCREEN_COMMAND).unwrap().shortcut),
            Some(FULLSCREEN_CHORD)
        );
        for workspace in BUILT_IN_WORKSPACES {
            assert!(
                find(&format!("{WORKSPACE_COMMAND_PREFIX}{}", workspace.id)).is_some(),
                "{}",
                workspace.id
            );
        }
    }

    #[test]
    fn escape_stops_play_before_it_leaves_full_screen() {
        use crate::shell::EscapeAction;
        assert_eq!(escape_action_full(true, false, true), EscapeAction::StopPlay);
        assert_eq!(
            escape_action_full(false, false, true),
            EscapeAction::LeaveFullscreen
        );
        assert_eq!(escape_action_full(false, true, true), EscapeAction::Ignore);
        assert_eq!(escape_action_full(false, false, false), EscapeAction::Close);
    }

    #[test]
    fn add_and_replace_move_a_panel_and_never_duplicate_it() {
        let mut dock = crate::editor_layout::default_dock();
        add_tab(&mut dock, Panel::Preferences, Some(Panel::Outliner)).unwrap();
        let path = dock.find_tab(&Panel::Preferences).unwrap();
        assert_eq!(
            path.node_path(),
            dock.find_tab(&Panel::Outliner).unwrap().node_path()
        );
        // Moving an open panel next to the inspector.
        add_tab(&mut dock, Panel::Content, Some(Panel::Inspector)).unwrap();
        assert_eq!(
            dock.find_tab(&Panel::Content).unwrap().node_path(),
            dock.find_tab(&Panel::Inspector).unwrap().node_path()
        );
        replace_tab(&mut dock, Panel::Inspector, Panel::Terrain).unwrap();
        assert!(dock.find_tab(&Panel::Inspector).is_none());
        assert_eq!(
            dock.iter_all_tabs()
                .filter(|(_, t)| **t == Panel::Terrain)
                .count(),
            1
        );
        assert_eq!(focused_tab(&dock), Some(Panel::Terrain));
        assert_eq!(
            replace_tab(&mut dock, Panel::Inspector, Panel::Kit)
                .unwrap_err()
                .code,
            "panel_not_open"
        );
        assert_eq!(
            add_tab(&mut dock, Panel::Kit, Some(Panel::Inspector))
                .unwrap_err()
                .code,
            "panel_not_open"
        );
    }

    #[test]
    fn drag_dwell_counts_frames_once_per_frame() {
        let mut dwell = TabDwell::default();
        let mut fired = 0;
        for frame in 0..=DWELL_FRAMES + 5 {
            // egui may run the UI twice in a frame: both passes report the same frame number.
            for _ in 0..2 {
                fired += usize::from(dwell.note(Panel::Content, frame));
            }
        }
        assert_eq!(fired, 1);
        // A gap (the pointer left the tab) restarts the count.
        let mut dwell = TabDwell::default();
        assert!(!dwell.note(Panel::Content, 0));
        assert!(!dwell.note(Panel::Content, DWELL_FRAMES + 1));
        // Another tab restarts too.
        for frame in 0..DWELL_FRAMES {
            assert!(!dwell.note(
                if frame.is_multiple_of(2) {
                    Panel::Content
                } else {
                    Panel::OutputLog
                },
                frame
            ));
        }
    }
    #[test]
    fn joins_follow_edges_not_only_siblings_and_reject_diagonal_areas() {
        let mut dock = DockState::new(vec![Panel::Viewport]);
        let [left, right] =
            dock.main_surface_mut()
                .split_right(NodeIndex::root(), 0.5, vec![Panel::Inspector]);
        dock.main_surface_mut()
            .split_below(left, 0.5, vec![Panel::Content]);
        dock.main_surface_mut()
            .split_below(right, 0.5, vec![Panel::Outliner]);
        let before = LayoutDoc::from_dock(&dock);
        assert_eq!(
            join_areas(&mut dock, Panel::Viewport, Panel::Outliner)
                .unwrap_err()
                .code,
            "bad_pane_request"
        );
        assert_eq!(LayoutDoc::from_dock(&dock), before);
        join_areas(&mut dock, Panel::Viewport, Panel::Inspector).unwrap();
        let path = dock.find_tab(&Panel::Inspector).unwrap();
        assert_eq!(
            dock.leaf(path.node_path()).unwrap().tabs,
            vec![Panel::Inspector, Panel::Viewport]
        );
        assert_eq!(dock.iter_leaves().count(), 3);
    }

    #[test]
    fn swap_keeps_geometry_and_moves_active_tabs_across_surfaces() {
        let mut dock = DockState::new(vec![Panel::Viewport, Panel::Content]);
        let window = dock.add_window(vec![Panel::Inspector, Panel::Outliner]);
        let content = dock.find_tab(&Panel::Content).unwrap();
        dock.set_active_tab(content).unwrap();
        let outliner = dock.find_tab(&Panel::Outliner).unwrap();
        dock.set_active_tab(outliner).unwrap();
        let before = LayoutDoc::from_dock(&dock);
        assert_eq!(
            join_areas(&mut dock, Panel::Viewport, Panel::Inspector)
                .unwrap_err()
                .code,
            "bad_pane_request"
        );
        assert_eq!(LayoutDoc::from_dock(&dock), before);
        swap_areas(&mut dock, Panel::Viewport, Panel::Inspector).unwrap();
        assert_eq!(dock.find_tab(&Panel::Viewport).unwrap().surface, window);
        let moved = dock
            .leaf(dock.find_tab(&Panel::Content).unwrap().node_path())
            .unwrap();
        assert_eq!(moved.tabs[moved.active.0], Panel::Content);
        let main = dock.leaf(egui_dock::NodePath::MAIN_ROOT).unwrap();
        assert_eq!(main.tabs[main.active.0], Panel::Outliner);
    }

    #[test]
    fn workspace_ui_requests_apply_once_per_frame() {
        let ctx = egui::Context::default();
        let mut world = World::new();
        let request = ViewRequest::Join {
            from: Panel::Viewport,
            into: Panel::Inspector,
        };
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            queue_ui(ui, &mut world, request.clone());
            queue_ui(ui, &mut world, request.clone());
        });
        output.textures_delta.clear();
        assert_eq!(world.resource::<ViewRequests>().pending, vec![request]);
    }
}
