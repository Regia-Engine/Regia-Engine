// SPDX-License-Identifier: MPL-2.0
//! The editor shell: the docking layout, menus, status bar and the panels' shared state
//! (`docs/specs/19-editor.md` § 2).
//!
//! Panels live as tabs in an `egui_dock` tree; the default layout is a center viewport flanked
//! by the outliner and inspector, with the content browser and output log below. The shell is
//! UI state only — everything the panels edit lives in the world's resources so the agent
//! protocol sees the same state.

use egui_dock::{DockArea, DockState, TabViewer};

use regia_ecs::prelude::World;
use regia_render::RenderDevice;

use crate::command::EditorCommand as _;
use crate::command::{SpawnEntities, UndoStack};
use crate::content::{ContentBrowserState, handle_viewport_drop};
use crate::level_io::{self, OpenLevel};
use crate::log_panel::LogPanelState;
use crate::material_editor::{MaterialEditorState, material_editor_panel};
use crate::outliner::outliner_panel;
use crate::project_settings::{ProjectSettingsState, project_settings_panel};
use crate::selection::Selection;
use crate::viewport::{ViewportSettings, viewport_panel};
/// One dockable panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Panel {
    /// The 3D viewport.
    Viewport,
    /// Entity hierarchy.
    Outliner,
    /// Reflected properties of the selection.
    Inspector,
    /// The project's assets.
    Content,
    /// The engine log ring.
    OutputLog,
    /// The `.material.ron` asset editor.
    Material,
    /// `Regia.toml` general settings.
    ProjectSettings,
    /// Mesh / texture / sound viewer.
    AssetViewer,
    /// The anim graph editor (blend tree, state machines, preview, PIE debugging).
    AnimGraph,
    /// The clip viewer (timeline, notifies, curves, root motion).
    AnimClip,
    /// The skeleton viewer (joints, sockets, IK chains).
    AnimSkeleton,
    /// The kit viewer (File ▸ Open kit…).
    Kit,
    /// The baked-GI Bake panel (volume, settings, bake button).
    Bake,
    /// The GI bake runner (`regia bake-gi` on a `.bake.ron` or built-in, with progress and cancel).
    BakeRunner,
    /// The lightmap baker (`regia bake-lightmaps`: progress, density preview, chart seams).
    Lightmaps,
    /// The runtime UI theme editor (`.theme.ron` tokens, preview, lint, save).
    Theme,
    /// The shot sequence curve editor (field of view, roll, depth-of-field tracks).
    Shots,
    /// Play-in-editor networking: mode, clients, conditions, stats overlay and bandwidth view.
    Net,
    /// Out-of-process editor tools: launch, stop, progress, each tool's own view (ADR-1523).
    Tool,
    /// The 2D mode: open a map and rule set, pick the layer and the brush (ADR-1889).
    Mode2d,
    /// The DCC scene feed (spec 60 § 6): listeners, sessions, items, bake, record and replay.
    DccFeed,
    /// The procedural graph editor (spec 67): canvas, palette, settings, run report, data table.
    ProcGraph,
    /// Content chunks (spec 40 § 6): chunk sizes, direct assets by rule, why an asset is in its chunk.
    ContentChunks,
    /// Discovered extensions: state, capabilities, diagnostics, enable and disable (spec 57).
    Extensions,
    /// A panel an extension registered: its id and title (leaked once per panel).
    ExtensionPanel(&'static str, &'static str),
    /// The sequence curve editor (`.seq.ron` v2 float and vec3 tracks, Bezier tangents, ADR-1880).
    SeqCurves,
    /// The UI designer: WYSIWYG `.ui.ron` editing with view-model bindings (ADR-1811).
    UiDesigner,
    /// The UI animation editor: `.uianim.ron` tracks, keys, scrub and save (ADR-3092).
    UiAnim,
    /// The pre-fracture tool: chunks, bonds, preview, save and `.meta` cook settings.
    Fracture,
    /// Curve tables and choosers: graph, grid, test context (spec 51 § 6, ADR-2031).
    Tables,
    /// Attributes, live effects and grants of an entity (spec 53 § 6, ADR-2031).
    Abilities,
    /// The montage timeline: slot tracks, sections, notifies, warps (spec 34 M10, ADR-2031).
    Montage,
    /// A character's physics asset: ragdoll bodies and joint limits (ADR-2031).
    Ragdoll,
    /// The asset validators' findings (spec 32 § 8).
    Problems,
    /// The columns of what uses what (spec 32 § 8).
    ReferenceGraph,
    /// The treemap of what a closure costs (spec 32 § 8).
    SizeMap,
    /// The open level's data layers (spec 37).
    Layers,
    /// The open level's functional tests (spec 41 § 6).
    Tests,
    /// Live debugging of state trees: active path, transitions, blackboard, trace (ADR-2030).
    StateTrees,
    /// Smart objects and slots with their claim state (ADR-2030).
    SmartObjects,
    /// Camera assets: the asset page, the rig graph, running drivers (ADR-2030).
    Cameras,
    /// The material graph editor (node canvas, live preview, errors on nodes; ADR-2318).
    MaterialGraph,
    /// The audio mixer: bus tree, faders, mute, solo and live meters (spec 16 § 5, ADR-2530).
    AudioMixer,
    /// The sound cue editor: variations, weights, steal policy, ranges, audition (spec 16 § 5, ADR-2530).
    CueEditor,
    /// Staged changes agents proposed, with diffs and accept/reject (specs/23 § 5, ADR-2346).
    AgentChanges,
    /// A user-configured external agent conversation.
    Agent,
    /// Editor preferences and the keymap (ADR-2321); opened from Edit ▸ Preferences.
    Preferences,
    /// The Terrain panel: the level's terrain, heightmap import/export, grass density and
    /// `.scatter.ron` rules (ADR-2411).
    Terrain,
    /// The behavior tree editor: outline, palette, inspector, live node status (ADR-2408).
    BehaviorTree,
    /// The motion-matching debug panel: candidates, cost terms, database (spec 52 § 6, ADR-2409).
    MotionMatching,
    /// The runtime UI Inspector: tree, search, layout-map pick, property sources (ADR-2703).
    UiInspector,
    /// The current level's `LevelSettings` (ADR-2696); opened from Edit ▸ World settings.
    WorldSettings,
    /// The VFX editor: module stacks, curve and gradient editors, live preview, per-emitter cost
    /// (spec 26 § 3, ADR-3171).
    VfxEditor,
    /// The state tree editor: node view, palette, inspector, live active path (ADR-3342).
    StateTreeEditor,
    /// The Video panel: preview, transport, stream info, decode stats, boot movie list (spec 50, ADR-2720).
    Video,
    /// The conversation graph editor: graph, edits with undo, findings, preview (ADR-3298).
    Conversation,
    /// The Localization panel: key-by-locale table, filters, edits, gather, usages, pseudo (ADR-3260).
    Localization,
    /// The cloth inspector and paint mode (spec 55 § 6, ADR-2884).
    Cloth,
    /// The Look-dev panel: a reference path trace of the level (specs/27 § 1.1, ADR-5896).
    LookDev,
    /// The data asset editor: a `.data.ron` value through the reflection inspector (spec 19 § 2,
    /// ADR-7234).
    DataAsset,
    /// The vehicle tuning panel: `.vehicle.ron` sections, plots, live Apply in play and recorded
    /// runs (ADR-9760).
    VehicleTuning,
    /// The Logic Graph canvas: event → condition → action rules of a `.logic.ron` (spec 102 § 6).
    LogicGraph,
    /// The smart-object definition editor: a `.smartobject.ron`'s rules, slots and entrances
    /// (spec 39 § 3, ADR-7736).
    SmartObjectDef,
    /// The Biome editor: a `.biome.ron`'s layers, entries, filters and grass as typed forms
    /// (spec 65 § 3, w11-biome-procgen-tooling).
    Biome,
    /// The Bulk Edit grid: many entities or data assets by one type's fields (ADR-7787).
    BulkEdit,
    /// Project Health: one ranked list of the project's checks with recommended actions (ADR-7789).
    ProjectHealth,
    /// The Retarget panel: source/target bone map by chain, rest-pose alignment, side-by-side
    /// preview and the "Retarget clips…" batch (ADR-7801).
    Retarget,
    /// The World Map: a streamed level's cells, loaded and unloaded in the edit world (P-241,
    /// ADR-7802).
    WorldMap,
    /// The shared undo/redo history and change details.
    UndoHistory,
    /// Debug data for the active world's primary selection.
    SelectedDebug,
    /// Per-entity recorded shapes and messages with frame scrubbing.
    VisualLog,
    /// Saved review threads attached to world markers.
    Notes,
    /// Agent calls and user commands in session order.
    Activity,
    /// Rule sets, playlists and the play-in-editor Match strip (ADR-8738).
    MatchRules,
    /// A Game tab: one PIE process's pictures, toolbar and forwarded input (ADR-7831). Not saved
    /// in layouts (the process is gone when the editor starts again).
    GameView(crate::pie_process::ProcessRole),
    /// A further instance of the Outliner, Content Browser or Inspector (`1..=`
    /// [`MAX_PANEL_INSTANCE`]), with its own search, columns, folder or lock (P-003, ADR-8105 (a)).
    /// Instance 0 is the plain variant ([`Panel::Outliner`], [`Panel::Content`], [`Panel::Inspector`]).
    Instance(InstancedPanel, u8),
    /// A utility panel (ADR-8189): a UI-designer document with a logic graph or Lua driver, by
    /// panel id and title (Window ▸ Utility Panels). Not saved in layouts.
    UtilityPanel(&'static str, &'static str),
}

/// The highest instance number of an instanced panel (`outliner#7`): instance 0 and seven more.
pub const MAX_PANEL_INSTANCE: u8 = 7;

/// The panels that may have more than one instance (ADR-8105 (a)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum InstancedPanel {
    /// The Outliner: each instance has its own search and columns.
    Outliner,
    /// The Content Browser: each instance has its own folder and search.
    Content,
    /// The Inspector: each instance has its own lock.
    Inspector,
}

impl InstancedPanel {
    /// Every instanced panel, in menu order.
    pub const ALL: [Self; 3] = [Self::Outliner, Self::Content, Self::Inspector];

    /// Instance 0 of this panel.
    #[must_use]
    pub fn base(self) -> Panel {
        match self {
            Self::Outliner => Panel::Outliner,
            Self::Content => Panel::Content,
            Self::Inspector => Panel::Inspector,
        }
    }

    /// The instanced panel `panel` is an instance of (any instance number).
    #[must_use]
    pub fn of(panel: Panel) -> Option<Self> {
        match panel {
            Panel::Outliner => Some(Self::Outliner),
            Panel::Content => Some(Self::Content),
            Panel::Inspector => Some(Self::Inspector),
            Panel::Instance(kind, _) => Some(kind),
            _ => None,
        }
    }

    /// Instance `n` of this panel (`0`: the plain variant; `None` above [`MAX_PANEL_INSTANCE`]).
    #[must_use]
    pub fn instance(self, n: u8) -> Option<Panel> {
        match n {
            0 => Some(self.base()),
            1..=MAX_PANEL_INSTANCE => Some(Panel::Instance(self, n)),
            _ => None,
        }
    }

    /// The menu and palette label of a new instance ("New Outliner").
    #[must_use]
    pub fn new_label(self) -> &'static str {
        match self {
            Self::Outliner => "New Outliner",
            Self::Content => "New Content Browser",
            Self::Inspector => "New Inspector",
        }
    }
}

/// Tab titles of the instances `1..=7` ("Outliner 2" is `outliner#1`, the second Outliner).
const INSTANCE_TITLES: [[&str; MAX_PANEL_INSTANCE as usize]; 3] = [
    [
        "Outliner 2",
        "Outliner 3",
        "Outliner 4",
        "Outliner 5",
        "Outliner 6",
        "Outliner 7",
        "Outliner 8",
    ],
    [
        "Content 2",
        "Content 3",
        "Content 4",
        "Content 5",
        "Content 6",
        "Content 7",
        "Content 8",
    ],
    [
        "Inspector 2",
        "Inspector 3",
        "Inspector 4",
        "Inspector 5",
        "Inspector 6",
        "Inspector 7",
        "Inspector 8",
    ],
];

/// Layout ids of the instances `1..=7` (`<panel id>#<n>`, ADR-8105 (a)).
const INSTANCE_IDS: [[&str; MAX_PANEL_INSTANCE as usize]; 3] = [
    [
        "outliner#1",
        "outliner#2",
        "outliner#3",
        "outliner#4",
        "outliner#5",
        "outliner#6",
        "outliner#7",
    ],
    [
        "content#1",
        "content#2",
        "content#3",
        "content#4",
        "content#5",
        "content#6",
        "content#7",
    ],
    [
        "inspector#1",
        "inspector#2",
        "inspector#3",
        "inspector#4",
        "inspector#5",
        "inspector#6",
        "inspector#7",
    ],
];

/// The `[kind][n - 1]` entry of an instance table (instance numbers out of range clamp; the enum
/// never holds one, `InstancedPanel::instance` refuses them).
fn instance_entry(
    table: &[[&'static str; MAX_PANEL_INSTANCE as usize]; 3],
    kind: InstancedPanel,
    n: u8,
) -> &'static str {
    let row = match kind {
        InstancedPanel::Outliner => 0,
        InstancedPanel::Content => 1,
        InstancedPanel::Inspector => 2,
    };
    let column = usize::from(n.clamp(1, MAX_PANEL_INSTANCE) - 1);
    table[row][column]
}

impl Panel {
    /// The instance number of this tab (0 for every panel but [`Panel::Instance`]).
    #[must_use]
    pub fn instance(self) -> u8 {
        match self {
            Self::Instance(_, n) => n,
            _ => 0,
        }
    }

    /// Instance 0 of this panel (itself for every panel but [`Panel::Instance`]).
    #[must_use]
    pub fn base(self) -> Panel {
        match self {
            Self::Instance(kind, _) => kind.base(),
            other => other,
        }
    }

    /// The layout id of an instance tab (`outliner#1`).
    pub(crate) fn instance_id(kind: InstancedPanel, n: u8) -> &'static str {
        instance_entry(&INSTANCE_IDS, kind, n)
    }
    /// The tab title.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Viewport => "Viewport",
            Self::Outliner => "Outliner",
            Self::Inspector => "Inspector",
            Self::Content => "Content",
            Self::OutputLog => "Output Log",
            Self::Material => "Material",
            Self::ProjectSettings => "Project Settings",
            Self::AssetViewer => "Asset Viewer",
            Self::AnimGraph => "Anim Graph",
            Self::AnimClip => "Clip",
            Self::AnimSkeleton => "Skeleton",
            Self::Kit => "Kit",
            Self::Bake => "Bake",
            Self::BakeRunner => "Bake runner",
            Self::Lightmaps => "Lightmaps",
            Self::Theme => "Theme",
            Self::Shots => "Shots",
            Self::Net => "Net",
            Self::Tool => "Tools",
            Self::Mode2d => "2D",
            Self::DccFeed => "DCC Feed",
            Self::ProcGraph => "Procedural Graph",
            Self::ContentChunks => "Content Chunks",
            Self::Extensions => "Extensions",
            Self::ExtensionPanel(_, title) => title,
            Self::SeqCurves => "Sequence Curves",
            Self::UiDesigner => "UI Designer",
            Self::UiAnim => "UI Animation",
            Self::Fracture => "Fracture",
            Self::Tables => "Tables",
            Self::Abilities => "Abilities",
            Self::Montage => "Montage",
            Self::Ragdoll => "Ragdoll",
            Self::Problems => "Problems",
            Self::ReferenceGraph => "Reference Graph",
            Self::SizeMap => "Size Map",
            Self::Layers => "Layers",
            Self::Tests => "Tests",
            Self::StateTrees => "State Trees",
            Self::SmartObjects => "Smart Objects",
            Self::Cameras => "Cameras",
            Self::MaterialGraph => "Material Graph",
            Self::AudioMixer => "Audio Mixer",
            Self::CueEditor => "Cue Editor",
            Self::AgentChanges => "Agent Changes",
            Self::Agent => "Agent",
            Self::Preferences => "Preferences",
            Self::Terrain => "Terrain",
            Self::BehaviorTree => "Behavior Tree",
            Self::MotionMatching => "Motion Matching",
            Self::UiInspector => "UI Inspector",
            Self::WorldSettings => "World Settings",
            Self::VfxEditor => "VFX Editor",
            Self::StateTreeEditor => "State Tree Editor",
            Self::Video => "Video",
            Self::Conversation => "Conversation",
            Self::Localization => "Localization",
            Self::Cloth => "Cloth",
            Self::LookDev => "Look-dev",
            Self::DataAsset => "Data Asset",
            Self::VehicleTuning => "Vehicle Tuning",
            Self::LogicGraph => "Logic Graph",
            Self::SmartObjectDef => "Smart Object Definition",
            Self::Biome => "Biome",
            Self::BulkEdit => "Bulk Edit",
            Self::ProjectHealth => "Project Health",
            Self::Retarget => "Retarget",
            Self::WorldMap => "World Map",
            Self::UndoHistory => "Undo History",
            Self::SelectedDebug => "Selected Object Debug",
            Self::VisualLog => "Visual Log",
            Self::Notes => "Notes",
            Self::Activity => "Activity",
            Self::MatchRules => "Match Rules",
            Self::GameView(_) => "Game",
            Self::Instance(kind, n) => instance_entry(&INSTANCE_TITLES, kind, n),
            Self::UtilityPanel(_, title) => title,
        }
    }
}

/// Where a panel's tab group sat on screen in the last frame ([`EditorShell::panel_area`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelArea {
    /// The whole group: tab bar and body.
    pub group: egui::Rect,
    /// The body the active tab draws into.
    pub body: egui::Rect,
    /// Whether the panel is the group's active tab (the one that drew).
    pub active: bool,
    /// Whether the group floats in its own window (undocked).
    pub floating: bool,
}

/// The editor shell: dock layout + panel state. `device` drives the viewport's background
/// rendering; without it the viewport still works (dark clear + grid + gizmos).
pub struct EditorShell {
    dock: DockState<Panel>,
    /// A restored layout's floating windows still need moving onto the screen (ADR-2771): kept
    /// until every window has been drawn once and clamped.
    clamp_floating: bool,
    /// Where the floating windows were drawn last frame (egui keeps them; the dock does not).
    window_rects: crate::editor_layout::WindowRects,
    log: LogPanelState,
    content: ContentBrowserState,
    material: MaterialEditorState,
    settings: ProjectSettingsState,
    viewport: crate::viewport::ViewportPanelState,
    asset_viewer: crate::asset_viewer::AssetViewerState,
    toast: String,
    device: Option<RenderDevice>,
    /// Crash-recovery autosaves to offer, one at a time (`specs/19` § 7).
    recovery: Vec<crate::autosave::RecoveryCandidate>,
    /// Documents a crashed session left in `.regia/autosave/docs/` (ADR-9770), offered with the
    /// level autosaves.
    recovery_docs: Vec<crate::doc_autosave::DocRecovery>,
    /// The `RecoveryOffers` revision the shell last published or adopted (`None`: not yet).
    offers_seen: Option<u64>,
    /// The shell changed its offers since it last published them.
    offers_changed: bool,
    /// The exit already saved the project's viewport thumbnail (the exit paths share one capture).
    exit_thumbnail_saved: bool,
    /// Whether the unsaved-changes modal shows over the shell.
    close_prompt: bool,
    /// The decision the close modal collected (taken by the host each frame).
    close_decision: Option<crate::autosave::CloseDecision>,
    /// The autosave interval (5 min default; tests shrink it).
    autosave_every: std::time::Duration,
    /// The launch screen shown until a project is open (`specs/19` § 6).
    browser: crate::project::ProjectBrowserState,
    /// Layout persistence (ADR-2321): the project root the layout was restored for (`Some(None)`:
    /// restored for "no project").
    layout_restored_for: Option<Option<std::path::PathBuf>>,
    /// Where the shown layout came from.
    layout_source: crate::editor_layout::LayoutSource,
    /// What the last restore dropped or added.
    layout_report: crate::editor_layout::RestoreReport,
    /// The preferences generation the shell last read (autosave interval).
    prefs_seen: Option<u64>,
    /// Crash reports from the previous run. Empty until [`EditorShell::set_pending_reports`].
    pending_reports: Vec<crate::crash_reports::ListedReport>,
    /// Text of the report the recovery dialog is showing.
    shown_report: Option<String>,
    /// Workspaces, maximize and full screen (ADR-7753).
    views: crate::workspaces::DockViews,
    /// View ▸ Layouts' text fields (ADR-7753).
    layout_naming: crate::workspaces::LayoutNaming,
    /// Floated panels as OS windows (ADR-7821).
    os: crate::os_windows::OsWindows,
    /// Content Browser instances and a restored layout's instance states (P-003, ADR-8105 (a)).
    instances: crate::panel_instances::ShellInstances,
}

/// The close prompt's line for a dirty shot list with no file (detached, or adopted from a
/// sequence player and never saved): Save and exit does not write it.
/// Size of the viewport render saved as a project's thumbnail, pixels (`save_project_thumbnail` shrinks
/// it to the stored size).
pub const PROJECT_THUMBNAIL_SIZE: [u32; 2] = [256, 144];

const UNTITLED_SHOT_LIST: &str =
    "an untitled shot list (not saved by Save and exit: save it from the Shots panel first)";

impl EditorShell {
    /// Builds the shell with the default layout.
    #[must_use]
    pub fn new(device: Option<RenderDevice>, project: Option<std::path::PathBuf>) -> Self {
        // The built-in layout; a saved one replaces it on the first frame (ADR-2321).
        let dock = crate::editor_layout::default_dock();
        // Crash recovery: autosaves left behind by a crashed session (specs/19 § 7), offered
        // one at a time as a modal when the shell first draws.
        let recovery: Vec<crate::autosave::RecoveryCandidate> = project
            .as_deref()
            .map(crate::autosave::recovery_candidates)
            .map(|candidates| {
                candidates
                    .into_iter()
                    .filter(crate::autosave::should_offer_recovery)
                    .collect()
            })
            .unwrap_or_default();
        let recovery_docs = project
            .as_deref()
            .map(crate::doc_autosave::doc_recovery_candidates)
            .unwrap_or_default();
        Self {
            dock,
            clamp_floating: true,
            window_rects: crate::editor_layout::WindowRects::new(),
            log: LogPanelState::default(),
            content: ContentBrowserState {
                project_root: project,
                ..Default::default()
            },
            material: MaterialEditorState::default(),
            settings: ProjectSettingsState::default(),
            viewport: crate::viewport::ViewportPanelState::default(),
            asset_viewer: crate::asset_viewer::AssetViewerState::default(),
            toast: String::new(),
            device,
            recovery,
            recovery_docs,
            offers_seen: None,
            offers_changed: true,
            exit_thumbnail_saved: false,
            close_prompt: false,
            close_decision: None,
            autosave_every: crate::autosave::AUTOSAVE_INTERVAL,
            browser: crate::project::ProjectBrowserState::default(),
            layout_restored_for: None,
            layout_source: crate::editor_layout::LayoutSource::Default,
            layout_report: crate::editor_layout::RestoreReport::default(),
            prefs_seen: None,
            pending_reports: Vec::new(),
            shown_report: None,
            views: crate::workspaces::DockViews::default(),
            layout_naming: crate::workspaces::LayoutNaming::default(),
            os: crate::os_windows::OsWindows::default(),
            instances: crate::panel_instances::ShellInstances::default(),
        }
    }

    /// Offers these crash reports in the recovery dialog. The host calls this after [`Self::new`]
    /// so a test that builds a shell does not pick up another test's report directory.
    pub fn set_pending_reports(&mut self, reports: Vec<crate::crash_reports::ListedReport>) {
        self.pending_reports = reports;
        self.shown_report = None;
    }

    /// Shows the unsaved-changes prompt (the host calls this when a close was requested while
    /// the level is dirty).
    pub fn prompt_close(&mut self) {
        self.close_prompt = true;
    }

    /// The close decision, once the user made one (the host takes it and exits).
    #[must_use]
    pub fn take_close_decision(&mut self) -> Option<crate::autosave::CloseDecision> {
        self.close_decision.take()
    }

    /// The crash-recovery autosaves still on offer (neither restored nor discarded). They are the
    /// only copy of a crashed session's edits, so an exit keeps them for the next launch.
    #[must_use]
    pub fn pending_recovery(&self) -> Vec<std::path::PathBuf> {
        self.recovery
            .iter()
            .map(|candidate| candidate.autosave.clone())
            .collect()
    }

    /// The exit's autosave cleanup: every autosave goes except the pending recovery ones (audit
    /// v033b: a clean exit used to delete the whole autosave folder, recovery window or not).
    pub fn exit_cleanup(&self) {
        if let Some(root) = self.project_root() {
            crate::autosave::clear_autosaves_except(&root, &self.pending_recovery());
            // The documents still on offer stay too (ADR-9770).
            let keep: Vec<String> = self.recovery_docs.iter().map(|d| d.entry.id.clone()).collect();
            if let Err(err) = crate::doc_autosave::clear_docs_except(&root, &keep) {
                tracing::warn!(target: "regia_editor", "document autosave cleanup: {err}");
            }
        }
    }

    /// Saves the main viewport as the open project's thumbnail, which the project browser's rows show
    /// (`recent_meta::thumbnail_source`). The picture is a [`PROJECT_THUMBNAIL_SIZE`] render of the
    /// perspective view in its game look, through the screenshot renderer slot (no pane's history is
    /// touched). Returns the file written.
    ///
    /// # Errors
    ///
    /// No project is open, there is no render device (a headless run without an adapter), the render
    /// failed or the file could not be written.
    pub fn save_project_thumbnail(&self, world: &mut World) -> Result<std::path::PathBuf, String> {
        let _span = tracing::info_span!(target: "regia_editor", "project_thumbnail").entered();
        let root = self.project_root().ok_or("no project is open")?;
        let device = self
            .device
            .clone()
            .or_else(|| world.get_resource::<RenderDevice>().cloned())
            .ok_or("no render device")?;
        let image = crate::screenshot::render(&device, world, PROJECT_THUMBNAIL_SIZE, false)
            .map_err(|err| err.message())?;
        let rgba: Vec<u8> = image
            .pixels
            .iter()
            .flat_map(|pixel| [pixel.r(), pixel.g(), pixel.b(), pixel.a()])
            .collect();
        crate::recent_meta::save_viewport_thumbnail(&root, image.width() as u32, image.height() as u32, &rgba)
    }

    /// The exit's thumbnail: once, and a failure only logs (a missing picture never holds an exit).
    fn save_exit_thumbnail(&mut self, world: &mut World) {
        if std::mem::replace(&mut self.exit_thumbnail_saved, true) || self.project_root().is_none() {
            return;
        }
        if let Err(err) = self.save_project_thumbnail(world) {
            tracing::debug!(target: "regia_editor", "project thumbnail not saved: {err}");
        }
    }

    /// The clean exit with the world: waits for queued autosave writes (bounded), then clears
    /// every autosave not on offer ([`Self::exit_cleanup`]).
    pub fn exit_cleanup_with(&mut self, world: &mut World) {
        self.save_exit_thumbnail(world);
        self.sync_offers(world);
        crate::doc_autosave::flush(world, std::time::Duration::from_secs(10));
        self.exit_cleanup();
    }

    /// The recovery window's Restore of the first pending candidate: the autosave becomes the
    /// open level with a fresh, dirty undo history ([`crate::autosave::apply_restore`]). Returns
    /// the status line.
    pub fn restore_first_recovery(&mut self, world: &mut World) -> Option<String> {
        let candidate = self.recovery.first().cloned()?;
        let toast = match crate::autosave::apply_restore(world, &candidate) {
            Ok(_) => {
                self.recovery.remove(0);
                self.offers_changed = true;
                format!("restored {}", candidate.autosave.display())
            }
            Err(err) => {
                tracing::error!(target: "regia_editor", "{err}");
                err
            }
        };
        self.toast = toast.clone();
        Some(toast)
    }

    /// The recovery window's Discard of the first pending candidate. The offer stays when the
    /// autosave file could not be deleted, and the toast is that IO error. A file that is
    /// already gone counts as discarded: the offer drops, or the recovery window — which has
    /// no close button — would stay up for the rest of the session.
    pub fn discard_first_recovery(&mut self) -> Option<String> {
        let candidate = self.recovery.first().cloned()?;
        if let Err(err) = std::fs::remove_file(&candidate.autosave)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            let toast = format!("could not discard {}: {err}", candidate.autosave.display());
            tracing::error!(target: "regia_editor", "{toast}");
            self.toast = toast.clone();
            return Some(toast);
        }
        self.recovery.remove(0);
        self.offers_changed = true;
        let toast = format!("discarded {}", candidate.autosave.display());
        self.toast = toast.clone();
        Some(toast)
    }

    /// Applies a Restore or Discard of the recovery list (`ids` of [`crate::doc_autosave::RecoveryRow`]),
    /// the same code path as `editor.recovery_restore` and `editor.recovery_discard`. Returns the
    /// status line.
    pub fn apply_recovery_action(
        &mut self,
        world: &mut World,
        action: &crate::doc_autosave::RecoveryListAction,
    ) -> String {
        self.sync_offers(world);
        let (verb, outcome) = match action {
            crate::doc_autosave::RecoveryListAction::Restore(ids) => {
                ("restored", crate::doc_autosave::restore(world, ids))
            }
            crate::doc_autosave::RecoveryListAction::Discard(ids) => {
                ("discarded", crate::doc_autosave::discard(world, ids))
            }
        };
        self.sync_offers(world);
        let mut toast = format!("{verb} {} item(s)", outcome.done.len());
        for (id, err) in &outcome.failed {
            tracing::error!(target: "regia_editor", "{id}: {err}");
            toast = format!("{toast}; {id}: {err}");
        }
        self.toast = toast.clone();
        toast
    }

    /// The documents with edits that are not on disk: the level (its undo stack), the Material
    /// panel's material, the Project Settings' `Regia.toml`, the UI designer's document, and
    /// each open material graph, function or instance. A material-graph edit does not mark the
    /// level dirty. A close asks before dropping any of them (audit v033b: only the level
    /// stack was checked).
    #[must_use]
    pub fn unsaved_documents(&self, world: &World) -> Vec<String> {
        let mut out = Vec::new();
        if world.get_resource::<UndoStack>().is_some_and(UndoStack::is_dirty) {
            out.push("the level".to_string());
        }
        if self.material.dirty {
            out.push(match &self.material.path {
                Some(path) => format!("material {}", path.display()),
                None => "the material".to_string(),
            });
        }
        if self.settings.is_dirty() {
            out.push("project settings (Regia.toml)".to_string());
        }
        if let Some(graphs) = world.get_resource::<crate::matgraph_editor::MatGraphEditor>() {
            out.extend(graphs.unsaved_labels());
        }
        if let Some(doc) = world
            .get_resource::<crate::ui_designer::UiDesigner>()
            .and_then(crate::ui_designer::UiDesigner::doc)
            .filter(|doc| doc.dirty())
        {
            out.push(match &doc.path {
                Some(path) => format!("UI document {}", path.display()),
                None => "an untitled UI document".to_string(),
            });
        }
        // The Shots panel's shot list (ADR-8730; rev-claude-57 seq-shot finding 2, ADR-7495). A
        // list with no file (detached, or adopted from a sequence player and never saved) is
        // listed, but Save and exit cannot write it (sprint-w9-rev-v06-lanes § 1 finding 9).
        if crate::shot_document::is_dirty(world) {
            out.push(match crate::shot_document::path(world) {
                Some(path) => format!("shot list {path}"),
                None => UNTITLED_SHOT_LIST.to_string(),
            });
        }
        out
    }

    /// The shell's own dirty documents for the Save all dialog: the Material panel's material and
    /// the Project Settings' `Regia.toml` (the world's documents come from the dialog itself).
    fn shell_dirty_rows(&self) -> Vec<crate::save_all_dialog::DirtyDoc> {
        let mut rows = Vec::new();
        if self.material.dirty {
            rows.push(crate::save_all_dialog::DirtyDoc {
                id: "shell:material".to_string(),
                kind: "material".to_string(),
                label: self.material.path.as_deref().map_or_else(
                    || "the material".to_string(),
                    |p| format!("material {}", p.display()),
                ),
                path: self.material.path.as_deref().map(|p| p.display().to_string()),
                can_save: self.material.path.is_some(),
                note: self
                    .material
                    .path
                    .is_none()
                    .then(|| "no file yet: save it from the Material panel".to_string()),
            });
        }
        if self.settings.is_dirty() {
            rows.push(crate::save_all_dialog::DirtyDoc {
                id: "shell:settings".to_string(),
                kind: "project_settings".to_string(),
                label: "project settings (Regia.toml)".to_string(),
                path: Some("Regia.toml".to_string()),
                can_save: self.project_root().is_some(),
                note: None,
            });
        }
        rows
    }

    /// Runs the shell rows' saves an agent asked for (`editor.save_all`, P-147, ADR-8147), then
    /// publishes the rows it lists next; returns them (the dialog's extra rows).
    fn sync_save_all_rows(&mut self, world: &mut World) -> Vec<crate::save_all_dialog::DirtyDoc> {
        let requests = crate::save_all_dialog::take_shell_requests(world);
        if !requests.is_empty() {
            let results = requests
                .iter()
                .map(|id| (id.clone(), self.save_shell_row(id).err()))
                .collect();
            crate::save_all_dialog::record_shell_results(world, results);
        }
        let rows = self.shell_dirty_rows();
        crate::save_all_dialog::publish_shell_rows(world, rows.clone());
        rows
    }

    /// Saves one of the shell's own rows ([`Self::shell_dirty_rows`]): the Material panel's
    /// material or the Project Settings' `Regia.toml`.
    fn save_shell_row(&mut self, id: &str) -> Result<(), String> {
        match id {
            "shell:material" => self.material.save(),
            "shell:settings" => match self.project_root() {
                Some(root) => self.settings.save(&root),
                None => Err("no project is open".to_string()),
            },
            _ => Ok(()),
        }
    }

    /// Acts on the Save all dialog's choice: saves the checked shell documents, and on a close
    /// hands the host its decision (the dialog already saved the checked world documents, so a
    /// close after **Save selected** exits without saving more).
    fn apply_save_all_decision(
        &mut self,
        world: &mut World,
        purpose: &crate::save_all_dialog::SavePurpose,
        decision: crate::save_all_dialog::SaveDecision,
    ) {
        use crate::autosave::CloseDecision;
        use crate::save_all_dialog::{SaveDecision, SavePurpose};
        let saved = match &decision {
            SaveDecision::Saved { extra } => {
                let mut failed = Vec::new();
                for id in extra {
                    if let Err(e) = self.save_shell_row(id) {
                        failed.push(format!("{id}: {e}"));
                    }
                }
                if failed.is_empty() {
                    self.toast = "Saved".into();
                    true
                } else {
                    self.toast = format!("Save failed: {}", failed.join("; "));
                    false
                }
            }
            _ => false,
        };
        // Save all (and Save selected on a close) refreshes the project's thumbnail (w13).
        if saved
            && !matches!(purpose, SavePurpose::Close)
            && let Err(err) = self.save_project_thumbnail(world)
        {
            tracing::debug!(target: "regia_editor", "project thumbnail not saved: {err}");
        }
        let go_ahead = matches!(decision, SaveDecision::DontSave)
            || (saved && matches!(decision, SaveDecision::Saved { .. }));
        match purpose {
            // A dirty document the dialog cannot save (an untitled material, UI document or shot
            // list, an editor with no save hook) keeps the editor open after **Save selected**:
            // only **Don't save** closes without it (sprint-w12-rev-g10 row 4). The dialog stays
            // up and says which rows are left.
            SavePurpose::Close if saved => {
                let left: Vec<String> = crate::save_all_dialog::dirty_documents(world)
                    .into_iter()
                    .chain(self.shell_dirty_rows())
                    .filter(|row| !row.can_save)
                    .map(|row| {
                        format!(
                            "{}: not saved ({}); save it there, or choose Don't save to close without it",
                            row.label,
                            row.note.as_deref().unwrap_or("this dialog cannot save it")
                        )
                    })
                    .collect();
                if left.is_empty() {
                    self.close_decision = Some(CloseDecision::DiscardAndExit);
                } else {
                    crate::save_all_dialog::open_dialog(world, SavePurpose::Close);
                    if let Some(mut dialog) =
                        world.get_resource_mut::<crate::save_all_dialog::SaveAllDialog>()
                    {
                        dialog.errors = left;
                    }
                }
            }
            SavePurpose::Close => {
                self.close_decision = Some(if go_ahead {
                    CloseDecision::DiscardAndExit
                } else {
                    CloseDecision::Cancel
                });
            }
            SavePurpose::LevelSwitch(switch) if go_ahead => self.toast = Self::switch_level(world, switch),
            SavePurpose::LevelSwitch(_) | SavePurpose::SaveAll => {}
        }
    }

    /// Saves the documents other than the level that have unsaved edits (the close modal's
    /// "Save and exit"; the host saves the level itself).
    ///
    /// # Errors
    ///
    /// The first save that failed; the exit must not go ahead.
    pub fn save_other_documents(&mut self, world: &mut World) -> Result<(), String> {
        // A document someone else locks (git LFS) refuses the save (ADR-7864).
        crate::vcs::locks::guard_documents(world)?;
        if self.material.dirty {
            self.material.save()?;
        }
        if self.settings.is_dirty() {
            let root = self
                .project_root()
                .ok_or("project settings have edits but no project is open")?;
            self.settings.save(&root)?;
        }
        if let Some(mut designer) = world.get_resource_mut::<crate::ui_designer::UiDesigner>()
            && designer.doc().is_some_and(crate::ui_designer::UiDesign::dirty)
        {
            designer
                .doc_mut()
                .and_then(|doc| doc.save(None))
                .map_err(|e| format!("UI document: {e}"))?;
        }
        if let Some(mut graphs) = world.get_resource_mut::<crate::matgraph_editor::MatGraphEditor>()
            && !graphs.unsaved_labels().is_empty()
        {
            graphs.save_dirty_documents()?;
        }
        // A shot list with no file is not saved here: the prompt said so (`UNTITLED_SHOT_LIST`),
        // and the exit goes ahead without it, as Discard would.
        if crate::shot_document::is_dirty(world) && crate::shot_document::path(world).is_some() {
            crate::shot_document::save_shot_list(world, None)
                .map_err(|e| format!("shot list: {} ({})", e.message, e.code))?;
        }
        Ok(())
    }

    /// `quit.request`: the agent's exit never prompts, so a dirty level is autosaved now (the
    /// 5-minute tick alone could be up to one interval behind) and kept for the next launch;
    /// a clean level clears the autosaves like any clean exit. Returns whether the session was
    /// clean (audit v033b: unsaved edits since the last tick were lost).
    pub fn prepare_agent_quit(&mut self, world: &mut World) -> bool {
        let others: Vec<String> = self
            .unsaved_documents(world)
            .into_iter()
            .filter(|doc| doc != "the level")
            .collect();
        if !others.is_empty() {
            tracing::warn!(target: "regia_editor", "quit requested; unsaved and not recoverable: {}", others.join(", "));
        }
        let Some(root) = self.project_root() else {
            return !world.get_resource::<UndoStack>().is_some_and(UndoStack::is_dirty);
        };
        self.save_exit_thumbnail(world);
        // The asset editors' dirty documents are kept for the next launch too (ADR-9770),
        // written here after any queued tick.
        self.sync_offers(world);
        crate::doc_autosave::flush(world, std::time::Duration::from_secs(10));
        let docs = crate::doc_autosave::take_snapshots(world);
        let docs_kept = !docs.is_empty();
        if docs_kept && let Err(err) = crate::doc_autosave::write_docs(&root, &docs) {
            tracing::error!(target: "regia_editor", "quit requested; the document autosave failed: {err}");
        }
        // The recovery window may still offer the file this level autosaves to: writing over it
        // would destroy the crashed session's only copy before anyone chose, the write the
        // autosave tick refuses too (ADR-7412). This session's edits go to a file beside it
        // (`<name>~quit.level.ron`), so the next launch offers both (rc2 review, editor-undo
        // finding 2).
        let taken: Vec<std::path::PathBuf> = self
            .recovery
            .iter()
            .map(|candidate| candidate.autosave.clone())
            .collect();
        match crate::autosave::autosave_unsaved_level_beside(world, &root, &taken) {
            None if docs_kept => false,
            None => {
                self.exit_cleanup_with(world);
                true
            }
            Some(Ok(path)) => {
                tracing::info!(target: "regia_editor", "quit requested with unsaved level edits; autosaved to {}", path.display());
                false
            }
            Some(Err(err)) => {
                tracing::error!(target: "regia_editor", "quit requested; the level autosave failed: {err}");
                false
            }
        }
    }

    /// Mirrors the Material panel into the world for `editor.edit_material`, and adopts a file
    /// that call just saved when the panel has it open and clean.
    fn sync_material_panel(&mut self, world: &mut World) {
        // The panel's edits, Undo and Redo use the shared editor history (P-322).
        crate::material_editor::sync_shared_history(world, &mut self.material);
        if let Some(reload) = world.remove_resource::<crate::material_editor::MaterialPanelReload>() {
            let same = self
                .material
                .path
                .as_deref()
                .is_some_and(|open| crate::material_editor::same_material_file(open, &reload.path));
            if same && !self.material.dirty {
                self.material.adopt_saved(reload.material);
                self.material.toast = format!("{} changed by an agent", reload.path.display());
            } else if same {
                self.material.toast = format!(
                    "{} changed on disk by an agent while this panel has unsaved edits",
                    reload.path.display()
                );
            }
        }
        self.publish_material_status(world);
    }

    fn publish_material_status(&self, world: &mut World) {
        let status = crate::material_editor::MaterialPanelStatus {
            path: self.material.path.clone(),
            dirty: self.material.dirty,
        };
        if world.get_resource::<crate::material_editor::MaterialPanelStatus>() != Some(&status) {
            world.insert_resource(status);
        }
    }

    /// Brings `panel`'s tab forward, adding it to the focused tab group when it is closed.
    pub fn open_panel(&mut self, panel: Panel) {
        // Another tab maximized: the panel opens in the restored dock, never in the one-tab dock
        // that a restore would drop (ADR-7753).
        if self.views.maximized().is_some_and(|shown| shown != panel) {
            self.views.restore(&mut self.dock);
        }
        match self.dock.find_tab(&panel) {
            Some(path) => {
                let _ = self.dock.set_active_tab(path);
            }
            None => self.dock.push_to_focused_leaf(panel),
        }
    }

    /// The shown layout as a [`crate::editor_layout::LayoutDoc`].
    #[must_use]
    pub fn layout(&self) -> crate::editor_layout::LayoutDoc {
        // A maximized tab shows alone; the layout is the dock it replaced (ADR-7753).
        crate::editor_layout::LayoutDoc::from_dock_with(self.views.base_dock(&self.dock), &self.window_rects)
    }

    /// The shown layout with each open panel instance's state ([`Self::layout`] plus `instances`,
    /// ADR-8105 (a)): only the tabs the layout names keep an entry.
    #[must_use]
    pub fn layout_with(&self, world: &World) -> crate::editor_layout::LayoutDoc {
        let mut doc = self.layout();
        if let Some(states) = world.get_resource::<crate::panel_instances::PanelInstanceStates>() {
            let mut named = doc.panel_ids();
            named.extend(
                doc.floating
                    .iter()
                    .flat_map(|w| w.tabs.iter().map(String::as_str)),
            );
            doc.instances = states
                .states
                .iter()
                .filter(|(id, _)| named.contains(&id.as_str()))
                .map(|(id, state)| (id.clone(), state.clone()))
                .collect();
        }
        doc
    }

    /// Opens a new panel instance (Window ▸ New Outliner, `editor.panel_new_instance`) with fresh
    /// state, in its panel's tab group when that is open (else the focused group). An instance
    /// already open is brought forward instead.
    pub fn open_new_instance(&mut self, panel: Panel) {
        if !matches!(panel, Panel::Instance(..)) {
            self.open_panel(panel);
            return;
        }
        if self.views.maximized().is_some() {
            self.views.restore(&mut self.dock);
        }
        if let Some(path) = self.dock.find_tab(&panel) {
            let _ = self.dock.set_active_tab(path);
            return;
        }
        self.instances.content.remove(&panel.instance());
        if let Some(path) = self.dock.find_tab(&panel.base()) {
            self.dock.set_focused_node_and_surface(egui_dock::NodePath {
                surface: path.surface,
                node: path.node,
            });
        }
        self.dock.push_to_focused_leaf(panel);
    }

    /// The lowest free instance of `kind` in the shown dock (`None`: all eight are open).
    #[must_use]
    pub fn free_instance(&self, kind: InstancedPanel) -> Option<Panel> {
        let open: Vec<Panel> = self
            .views
            .base_dock(&self.dock)
            .iter_all_tabs()
            .map(|(_, tab)| *tab)
            .collect();
        crate::panel_instances::free_instance(kind, &open).map(|n| Panel::Instance(kind, n))
    }

    /// Applies a restored layout's instance states, then publishes what every open instance shows
    /// and forgets the state of instances whose tab is gone (closing an instance drops its state).
    fn sync_instances(&mut self, ctx: &egui::Context, world: &mut World) {
        use crate::editor_layout::PanelInstanceState;
        use crate::panel_instances as pi;
        if let Some(states) = self.instances.pending.take() {
            self.restore_instances(ctx, world, &states);
        }
        let open: Vec<Panel> = self
            .views
            .base_dock(&self.dock)
            .iter_all_tabs()
            .map(|(_, tab)| *tab)
            .collect();
        let mut states = std::collections::BTreeMap::new();
        for kind in InstancedPanel::ALL {
            for n in 0..=MAX_PANEL_INSTANCE {
                let Some(panel) = kind.instance(n) else {
                    continue;
                };
                if !open.contains(&panel) {
                    if n > 0 {
                        self.drop_instance(ctx, world, kind, n);
                    }
                    continue;
                }
                let mut state = PanelInstanceState::default();
                match kind {
                    InstancedPanel::Outliner => {
                        state.filter = ctx
                            .data(|d| d.get_temp::<String>(pi::instance_id(pi::OUTLINER_FILTER_KEY, n)))
                            .unwrap_or_default();
                        if n > 0
                            && let Some(view) = crate::outliner_columns::current_view_state(world)
                            && let Some(set) = view.trees.get(panel.id())
                        {
                            state.columns = set.shown.iter().map(|c| c.name().to_owned()).collect();
                        }
                    }
                    InstancedPanel::Content => {
                        let browser = if n == 0 {
                            Some(&self.content)
                        } else {
                            self.instances.content.get(&n)
                        };
                        if let Some(browser) = browser {
                            state.filter = browser.search.clone();
                            state.folder = browser.folder.clone();
                        }
                    }
                    InstancedPanel::Inspector => {
                        state.locked = pi::inspector_lock(world, n).and_then(|e| pi::locked_text(world, e));
                    }
                }
                if !state.is_empty() {
                    states.insert(panel.id().to_owned(), state);
                }
            }
        }
        pi::PanelInstanceStates::publish(world, states);
    }

    /// Forgets what instance `n` (`>= 1`) of `kind` kept: its tab is gone.
    fn drop_instance(&mut self, ctx: &egui::Context, world: &mut World, kind: InstancedPanel, n: u8) {
        use crate::panel_instances as pi;
        match kind {
            InstancedPanel::Outliner => {
                let id = pi::instance_id(pi::OUTLINER_FILTER_KEY, n);
                if ctx.data(|d| d.get_temp::<String>(id)).is_some() {
                    ctx.data_mut(|d| d.remove::<String>(id));
                }
            }
            InstancedPanel::Content => {
                self.instances.content.remove(&n);
            }
            InstancedPanel::Inspector => {}
        }
        pi::drop_world_state(world, kind, n);
    }

    /// Gives each restored instance tab its saved state (`states` by tab id; an id naming no
    /// instanced panel is ignored).
    fn restore_instances(
        &mut self,
        ctx: &egui::Context,
        world: &mut World,
        states: &std::collections::BTreeMap<String, crate::editor_layout::PanelInstanceState>,
    ) {
        use crate::panel_instances as pi;
        for (id, state) in states {
            let Some(panel) = Panel::from_id(id) else {
                continue;
            };
            let Some(kind) = InstancedPanel::of(panel) else {
                continue;
            };
            let n = panel.instance();
            match kind {
                InstancedPanel::Outliner => {
                    let key = pi::instance_id(pi::OUTLINER_FILTER_KEY, n);
                    ctx.data_mut(|d| d.insert_temp(key, state.filter.clone()));
                    if n > 0 && !state.columns.is_empty() {
                        let shown: Vec<crate::outliner_columns::Column> =
                            crate::outliner_columns::Column::ALL
                                .into_iter()
                                .filter(|c| state.columns.iter().any(|name| name == c.name()))
                                .collect();
                        let tree = panel.id();
                        crate::outliner_columns::edit_view_state(world, |view| {
                            view.trees.insert(
                                tree.to_owned(),
                                crate::outliner_columns::ColumnSet {
                                    shown,
                                    ..Default::default()
                                },
                            );
                        });
                    }
                }
                InstancedPanel::Content => {
                    let root = self.content.project_root.clone();
                    let browser = if n == 0 {
                        &mut self.content
                    } else {
                        self.instances
                            .content
                            .entry(n)
                            .or_insert_with(|| ContentBrowserState {
                                project_root: root,
                                ..Default::default()
                            })
                    };
                    browser.folder.clone_from(&state.folder);
                    browser.search.clone_from(&state.filter);
                }
                InstancedPanel::Inspector => {
                    let entity = state
                        .locked
                        .as_deref()
                        .and_then(|text| pi::locked_entity(world, text));
                    pi::set_inspector_lock(world, n, entity);
                }
            }
        }
    }

    /// The shell's workspace, maximize and full-screen state (ADR-7753).
    #[must_use]
    pub fn views(&self) -> &crate::workspaces::DockViews {
        &self.views
    }

    /// Where the shown layout came from.
    #[must_use]
    pub fn layout_source(&self) -> crate::editor_layout::LayoutSource {
        self.layout_source
    }

    /// Where `panel`'s tab group sat on screen in the last drawn frame, or `None` when the panel is
    /// not in the layout or its group has not drawn yet. Drivers that send pointer input (the
    /// editor monkey test, ADR-3290) aim with it.
    #[must_use]
    pub fn panel_area(&self, panel: Panel) -> Option<PanelArea> {
        let path = self.dock.find_tab(&panel)?;
        let leaf = self.dock.leaf(path.node_path()).ok()?;
        (leaf.rect.is_positive() && leaf.rect.is_finite()).then_some(PanelArea {
            group: leaf.rect,
            body: leaf.viewport,
            active: leaf.active == path.tab,
            floating: !path.surface.is_main(),
        })
    }

    /// Replaces the dock and republishes the snapshot on the next frame.
    fn set_layout(&mut self, dock: DockState<Panel>, source: crate::editor_layout::LayoutSource) {
        self.dock = dock;
        self.views.forget_maximize();
        self.clamp_floating = true;
        self.layout_source = source;
    }

    /// Reads the preferences and keymap once, applies changed preferences, takes the autosave
    /// interval from them, and writes the files when they changed (ADR-2321).
    fn sync_preferences(&mut self, ctx: &egui::Context, world: &mut World) {
        crate::editor_prefs::load_prefs(world);
        crate::editor_keymap::load_keymap(world);
        crate::editor_prefs::apply_prefs(Some(ctx), world);
        if let Some(prefs) = world.get_resource::<crate::editor_prefs::EditorPreferences>() {
            let generation = prefs.generation();
            // Only a preference that is set moves the interval (tests shrink it directly).
            if self.prefs_seen != Some(generation) {
                if self.prefs_seen.is_some() || !prefs.is_default("autosave.interval_minutes") {
                    self.autosave_every =
                        crate::editor_prefs::autosave_interval(prefs).unwrap_or(std::time::Duration::MAX);
                }
                self.prefs_seen = Some(generation);
            }
        }
        crate::editor_prefs::persist_prefs(world);
        crate::editor_keymap::persist_keymap(world);
    }

    /// Restores the saved layout of the open project (or the user's default) once per project,
    /// when Preferences ▸ Layout ▸ Restore saved layout is on.
    fn restore_layout_once(&mut self, world: &mut World) {
        if crate::safe_mode::enabled(world) {
            return;
        }
        let root = self.project_root();
        if self.layout_restored_for.as_ref() == Some(&root) {
            return;
        }
        self.layout_restored_for = Some(root.clone());
        // The level and asset documents open when this project last closed (ADR-7753).
        if let Some(project) = root.as_deref()
            && let Some(config) = world
                .get_resource::<crate::editor_prefs::EditorConfigPaths>()
                .and_then(|paths| paths.dir.clone())
            && let Some(line) = crate::window_state::reopen_documents(world, &config, project)
        {
            self.toast = line;
        }
        let restore = world
            .get_resource::<crate::editor_prefs::EditorPreferences>()
            .is_none_or(|p| p.bool("layout.restore"));
        if !restore {
            return;
        }
        let user = world
            .get_resource::<crate::editor_prefs::EditorConfigPaths>()
            .and_then(crate::editor_prefs::EditorConfigPaths::user_layout_file);
        let Some((doc, source)) = crate::editor_layout::saved_layout_for(root.as_deref(), user.as_deref())
        else {
            return;
        };
        match doc.to_dock() {
            Ok((dock, report)) => {
                self.set_layout(dock, source);
                self.layout_report = report;
                // The instances' own state comes back with their tabs (ADR-8105 (a)).
                self.instances.pending = Some(doc.instances);
            }
            Err(err) => tracing::warn!(target: "regia_editor", "saved layout not restored: {err}"),
        }
    }

    /// Performs the layout actions agents and commands queued.
    fn apply_layout_requests(&mut self, world: &mut World) {
        use crate::editor_layout::{LayoutRequest, LayoutRequests, LayoutSource};
        let Some(requests) = world.remove_resource::<LayoutRequests>() else {
            return;
        };
        for request in requests.pending {
            match request {
                LayoutRequest::Reset => {
                    self.set_layout(crate::editor_layout::default_dock(), LayoutSource::Default)
                }
                LayoutRequest::Apply(doc) => match doc.to_dock() {
                    Ok((dock, report)) => {
                        self.set_layout(dock, LayoutSource::Session);
                        self.layout_report = report;
                        // The instances' own state comes back with their tabs (ADR-8105 (a)).
                        self.instances.pending = Some(doc.instances.clone());
                    }
                    Err(err) => self.toast = format!("layout not applied: {err}"),
                },
                LayoutRequest::Reload => {
                    self.layout_restored_for = None;
                    self.set_layout(crate::editor_layout::default_dock(), LayoutSource::Default);
                    self.restore_layout_once(world);
                }
                LayoutRequest::Open(panel) => self.open_panel(panel),
                LayoutRequest::NewInstance(panel) => self.open_new_instance(panel),
                LayoutRequest::SaveProject => self.toast = self.save_layout(world, false),
                LayoutRequest::SaveUser => self.toast = self.save_layout(world, true),
                LayoutRequest::RestoreRecovery => {
                    if self.restore_first_recovery(world).is_none() {
                        self.toast = "No autosave to restore".into();
                    }
                }
                LayoutRequest::DiscardRecovery => {
                    if self.discard_first_recovery().is_none() {
                        self.toast = "No autosave to discard".into();
                    }
                }
                LayoutRequest::MaterialUndo => {
                    self.material.toast = if self.material.undo() {
                        "Undone"
                    } else {
                        "Nothing to undo"
                    }
                    .into();
                }
                LayoutRequest::MaterialRedo => {
                    self.material.toast = if self.material.redo() {
                        "Redone"
                    } else {
                        "Nothing to redo"
                    }
                    .into();
                }
                LayoutRequest::MaterialReload => match self.material.path.clone() {
                    Some(path) => {
                        if let Err(err) = self.material.open(&path) {
                            self.material.toast = err;
                        }
                    }
                    None => self.material.toast = "No material is open".into(),
                },
            }
        }
    }

    /// Publishes [`crate::editor_layout::LayoutSnapshot`] when the dock changed.
    fn publish_layout(&mut self, world: &mut World) {
        let fingerprint = crate::editor_layout::dock_fingerprint(&self.dock)
            ^ crate::editor_layout::rects_fingerprint(&self.window_rects)
            ^ crate::panel_instances::PanelInstanceStates::revision_of(world).rotate_left(17);
        let current = world.get_resource::<crate::editor_layout::LayoutSnapshot>();
        if current.is_some_and(|s| s.fingerprint == fingerprint && s.source == self.layout_source) {
            return;
        }
        world.insert_resource(crate::editor_layout::LayoutSnapshot {
            doc: self.layout_with(world),
            fingerprint,
            source: self.layout_source,
            report: self.layout_report.clone(),
        });
    }

    /// Saves the shown layout into the project (`user` false) or as the user's default. Returns
    /// the status line.
    fn save_layout(&self, world: &World, user: bool) -> String {
        let path = if user {
            world
                .get_resource::<crate::editor_prefs::EditorConfigPaths>()
                .and_then(crate::editor_prefs::EditorConfigPaths::user_layout_file)
        } else {
            self.project_root()
                .map(|root| crate::editor_layout::project_layout_path(&root))
        };
        let Some(path) = path else {
            return if user {
                "No user config directory: the default layout is not saved".to_owned()
            } else {
                "Open a project to save its layout".to_owned()
            };
        };
        match crate::editor_layout::write_layout_file(&path, &self.layout_with(world)) {
            Ok(()) => format!("Layout saved to {}", path.display()),
            Err(err) => format!("Layout not saved: {err}"),
        }
    }

    /// The exit's layout save: into the project when Preferences ▸ Layout ▸ Save layout on exit
    /// is on (the default) and a project is open. Returns the file written.
    pub fn save_layout_on_exit(&mut self, world: &World) -> Option<std::path::PathBuf> {
        if crate::safe_mode::enabled(world) {
            return None;
        }
        let on = world
            .get_resource::<crate::editor_prefs::EditorPreferences>()
            .is_none_or(|p| p.bool("layout.save_on_exit"));
        let root = self.project_root()?;
        // The main window and its documents, for the next start of this project (ADR-7753).
        self.save_window_state(world, &root);
        if !on {
            return None;
        }
        let path = crate::editor_layout::project_layout_path(&root);
        match crate::editor_layout::write_layout_file(&path, &self.layout_with(world)) {
            Ok(()) => Some(path),
            Err(err) => {
                tracing::warn!(target: "regia_editor", "layout not saved on exit: {err}");
                None
            }
        }
    }

    /// Saves the main window's state, the open level and the open asset documents for project
    /// `root` into the user's `editor_window_state.ron` (ADR-7753).
    fn save_window_state(&self, world: &World, root: &std::path::Path) {
        let Some(config) = world
            .get_resource::<crate::editor_prefs::EditorConfigPaths>()
            .and_then(|paths| paths.dir.clone())
        else {
            return;
        };
        let assets: Vec<std::path::PathBuf> = self
            .material
            .path
            .iter()
            .cloned()
            .chain(
                world
                    .get_resource::<crate::anim::AnimEditor>()
                    .and_then(|anim| anim.graph_path.clone()),
            )
            .collect();
        let record = crate::window_state::capture(world, root, &assets);
        if let Err(err) = crate::window_state::save_for_project(&config, root, record) {
            tracing::warn!(target: "regia_editor", "window state not saved: {err}");
        }
    }

    /// Reloads a clean Project Settings panel after an agent saved `Regia.toml`.
    fn reload_project_settings(&mut self, world: &mut World) {
        if world
            .remove_resource::<crate::project_settings_ext::ProjectSettingsSaved>()
            .is_some()
            && !self.settings.is_dirty()
            && let Some(root) = self.project_root()
            && let Err(err) = self.settings.load(&root)
        {
            self.settings.toast = err;
        }
    }

    /// Shrinks the autosave interval (tests).
    #[cfg(test)]
    pub fn set_autosave_every(&mut self, every: std::time::Duration) {
        self.autosave_every = every;
    }

    /// Whether the shell has a GPU device for viewport rendering.
    #[must_use]
    pub fn has_device(&self) -> bool {
        self.device.is_some()
    }

    /// The project root the content browser and level I/O use.
    #[must_use]
    pub fn project_root(&self) -> Option<std::path::PathBuf> {
        self.content.project_root.clone()
    }

    /// Replaces the status-bar toast (hot-reload reports, panel actions).
    pub fn set_toast(&mut self, toast: String) {
        if !toast.is_empty() {
            self.toast = toast;
        }
    }

    /// Draws one full editor frame: menus, dock area, status bar.
    ///
    /// `play` is the play-in-editor world while a PIE session runs (`specs/19` § 4): the
    /// Viewport, Outliner and Inspector then view and edit the play world (changes are
    /// discarded on stop); the content browser and the File menu keep editing the edit world.
    #[tracing::instrument(target = "regia_editor", skip_all, name = "editor_shell_ui")]
    pub fn ui(&mut self, ui: &mut egui::Ui, world: &mut World, play: Option<&mut World>) {
        // The viewport writes a fresh aim only while the pointer is over a pane this frame.
        world.remove_resource::<crate::content::ViewportDropAim>();
        // Preferences and the keymap: read once, applied when they change (ADR-2321).
        self.sync_preferences(ui.ctx(), world);
        // The theme tokens in the world style the egui context (a change shows next frame).
        crate::theme::sync(ui.ctx(), world);
        crate::safe_mode::banner(ui, world);
        // The toolbar's preview language follows into the play world (ADR-7855).
        let mut play = play;
        crate::loc_panel::preview::sync_frame(ui.ctx(), world, play.as_deref_mut());
        // Observations belong to one UI pass, including a pass with no project/dock.
        world.insert_resource(crate::editor_layout::LayoutObservation::begin(ui.ctx()));
        // Share the window's device with `editor.capture_viewport` so the protocol and the
        // panel compile pipelines once. A headless capture may have stored one already;
        // replace it only when this shell owns a different device.
        if let Some(device) = &self.device {
            let stale = world
                .get_resource::<RenderDevice>()
                .is_none_or(|existing| existing.device_id() != device.device_id());
            if stale {
                world.insert_resource(device.clone());
            }
        }
        // An agent `editor.new_project` / `editor.open_project` writes `EditorProject` between
        // frames. Adopt it before deciding whether the dock or the project browser draws.
        self.adopt_project(world);
        crate::debug_colors::sync_play(world, play.as_deref_mut());
        // An open kit shows the dock without a project (a modder viewing kits needs none).
        let kit_open = world
            .get_resource::<crate::kit_view::KitView>()
            .is_some_and(crate::kit_view::KitView::is_open);
        if self.project_root().is_none() && !kit_open {
            self.draw_project_browser(ui, world);
            return;
        }

        let ctx = ui.ctx().clone();
        // The full-screen viewport hides the menus, toolbars and status bar (ADR-7753).
        let chrome = !self.views.chrome_hidden();
        if chrome {
            self.menu_bar(&ctx, ui, world);
        }
        crate::collab::show_window(&ctx, world);
        // The focused tab's panel picks the shortcut scope; F1 opens its help (ADR-7767).
        let focused = self.dock.find_active_focused().map(|(_, tab)| *tab);
        if world
            .get_resource::<crate::editor_keymap::FocusedPanel>()
            .map(|f| f.0)
            != Some(focused)
        {
            world.insert_resource(crate::editor_keymap::FocusedPanel(focused));
        }
        if let Some(status) = crate::command_palette::show(&ctx, world, play.is_some()) {
            self.toast = status;
        }
        // The Create prefab variant dialog, drawn here so it shows with the Content Browser and the
        // Outliner both closed (once per pass whichever caller asks first; ADR-7744).
        crate::prefab_variant_edit::variant_dialog(&ctx, world);
        // The drawer keys (Ctrl+Shift+Space, Esc) after the palette's user shortcuts and before the
        // view keys (ADR-7754).
        crate::drawers::handle_keys(&ctx, world);
        crate::console::poll(world);
        // Without the chrome (the full-screen viewport, ADR-7753) a drawer opens over the whole
        // screen's bottom edge.
        let mut status_rect = egui::Rect::from_min_size(ctx.content_rect().left_bottom(), egui::Vec2::ZERO);
        if chrome {
            self.prefab_mode_banner(ui, world);
            status_rect = self.status_bar(&ctx, ui, world);
            self.play_toolbar(ui, world);
        }

        self.restore_layout_once(world);
        self.apply_layout_requests(world);
        // Each open panel instance's state, for the layout file and `editor.layout_get` (P-003).
        self.sync_instances(&ctx, world);
        // Workspaces, maximize, full screen and pane changes (ADR-7753), then the feature profile.
        let hovered = ctx
            .pointer_hover_pos()
            .and_then(|pos| crate::workspaces::tab_at(&self.dock, pos))
            // A tab in an OS window is not under the main window's pointer (ADR-7821).
            .filter(|tab| !self.os.holds(&self.dock, *tab));
        crate::workspaces::handle_view_keys(&ctx, world, hovered);
        if let Some(line) = self.views.apply_requests(&mut self.dock, world) {
            self.toast = line;
        }
        self.views.sync_profile(&mut self.dock, world);
        // Floated panels as OS windows (ADR-7821): requests, then a serial per floated surface.
        let os_active = crate::os_windows::os_mode_active(&ctx, world);
        self.apply_os_window_requests(&ctx, world);
        self.os.sync(&self.dock, os_active, &ctx);
        // Asset documents reopening from the last session, one per frame (ADR-7753).
        if self.content.open_request.is_none()
            && let Some(path) = crate::window_state::next_reopen(world)
        {
            self.content.open_request = Some(path);
        }
        self.reload_project_settings(world);
        self.kit_focus(world);
        // A Project Health row's Problems link brings Problems forward (ADR-7789).
        if crate::project_health::take_focus_problems(world) {
            self.open_panel(Panel::Problems);
        }
        self.tool_focus(world);
        self.mode2d_focus(world);
        self.sync_extension_tabs(world);
        self.sync_game_tabs(world);
        self.sync_material_panel(world);
        // The asset editors' preview scenes: agent patches in, the panels' settings out (ADR-7771).
        crate::preview_scene::sync(
            world,
            &mut self.material.scene,
            &mut self.asset_viewer.scene,
            &mut self.asset_viewer.texture_scene,
        );
        if let Some(status) = crate::log_links::route_asset_requests(world, &mut self.content) {
            self.log.status = status;
        }

        // A double-clicked video opens in the Video panel; a file that cannot play shows its error there.
        if let Some(path) = self.content.video_request.take() {
            if let Err(e) = crate::video_panel::open_video(world, &path) {
                self.content.toast = Some(e.message);
            }
            self.open_panel(Panel::Video);
        }

        // Floating windows of a restored layout may sit off a smaller screen (ADR-2771). egui keeps
        // the rectangles; the clamp moves a window by queueing a position for the next frame.
        self.window_rects = crate::editor_layout::read_window_rects(&ctx, &self.dock);
        if os_active {
            // OS windows keep their own place; a lost monitor re-docks them instead (ADR-7821).
            self.os.fill_rects(&mut self.window_rects);
            self.clamp_floating = false;
        }
        if self.clamp_floating {
            let size = ctx.content_rect().size();
            let moved = crate::editor_layout::clamp_floating_windows(
                &mut self.dock,
                &self.window_rects,
                [size.x, size.y],
            );
            if moved > 0 {
                tracing::info!(target: "regia_editor", "moved {moved} floating window(s) back onto the screen");
            }
            self.clamp_floating =
                !crate::editor_layout::floating_windows_shown(&self.dock, &self.window_rects);
        }
        // The dock borrows `self`. It ends before the modals, which borrow the shell again.
        let dock_rect;
        {
            let EditorShell {
                dock,
                log,
                content,
                material,
                settings,
                viewport,
                asset_viewer,
                device,
                os,
                instances,
                ..
            } = self;
            if let Some(path) = content.view_request.take() {
                asset_viewer.open_request = Some(path);
            }

            // A double-clicked material opens in its editor before the panels draw; every other
            // kind opened through `crate::asset_open` already (ADR-7725).
            if let Some(path) = content.open_request.take()
                && let Err(err) = material.open(&path)
            {
                content.toast = Some(err);
            }

            // An audit action from the Content Browser's menus, and what a panel picked.
            if let Some(action) = content.asset_action.take() {
                content.toast = Some(crate::asset_panels::run_action(world, action));
            }
            if let Some(asset) = world
                .get_resource_mut::<crate::asset_panels::AssetPanels>()
                .and_then(|mut panels| panels.take_selected())
            {
                content.reveal(&asset);
            }

            // Reborrow `world` so the modals can use it after this block. `play` is moved
            // in; the dock is the only user.
            let world = &mut *world;
            let mut viewer = PanelViewer {
                world,
                play,
                log,
                content,
                material,
                settings,
                viewport,
                asset_viewer,
                device: device.as_ref(),
                in_os_window: false,
                instances,
            };
            let mut dock_style = crate::theme::dock_style(ui.style().as_ref());
            if dock_has_window(dock) {
                // egui_dock 0.21 ids the tab-bar scroll handle by node index alone, so two surfaces
                // with an overflowing group at the same index put one id on two layers (a panic
                // in debug builds; found by the monkey test, ADR-3290). With a floating window
                // open the bar is off; the wheel still scrolls the tabs.
                dock_style.tab_bar.show_scroll_bar_on_overflow = false;
            }
            // The add-tab popups list each group's tabs while the dock is borrowed (ADR-7753).
            crate::workspaces::DockGroups::refresh(viewer.world, dock);
            // Room for each tab bar's overflow button, drawn after the dock (ADR-7902).
            crate::workspaces::reserve_tab_buttons(&mut dock_style);
            let bar_height = dock_style.tab_bar.height;
            // The OS-window surfaces draw in their own windows' passes (ADR-7821).
            if os.active() {
                os.park(dock);
            }
            dock_rect = egui::CentralPanel::default()
                .show(ui, |ui| {
                    DockArea::new(dock).style(dock_style).show_inside(ui, &mut viewer);
                    crate::workspaces::tab_overflow_buttons(ui, viewer.world, dock, bar_height);
                })
                .response
                .rect;
            if os.active() {
                os.unpark(dock);
            }
            crate::workspaces::DockGroups::refresh(viewer.world, dock);
        }
        // The open drawer over the dock area (ADR-7754): the Content Browser's own state, or the
        // Output Log with the console line.
        let EditorShell { content, log, .. } = self;
        let mut link = None;
        let _ = crate::drawers::show(
            &ctx,
            world,
            dock_rect,
            status_rect,
            |ui, world, kind| match kind {
                crate::drawers::DrawerKind::Content => {
                    let mut open = world
                        .get_resource::<OpenLevel>()
                        .cloned()
                        .unwrap_or_else(|| level_io::new_level(world));
                    crate::content::content_browser_panel(ui, world, content, &mut open);
                    world.insert_resource(open);
                }
                crate::drawers::DrawerKind::Log => {
                    link = crate::log_panel::output_log_with_console(
                        ui,
                        log,
                        world,
                        crate::drawers::DRAWER_LINE,
                    );
                }
            },
        );
        if let Some(link) = link {
            self.log.status = crate::log_links::open_clicked(world, &link);
        }
        // A tab torn off this frame is a new OS window; every OS window is announced to egui
        // (its `viewport_output` lists them for the runner).
        if os_active {
            self.os.sync(&self.dock, true, &ctx);
            self.os.announce(&ctx, &self.dock);
        }
        let supported = world.contains_resource::<crate::os_windows::OsWindowSupport>();
        self.os.publish(world, &self.dock, supported);
        // A sequence preview ends when its panel closes (ADR-9710).
        crate::seq_tracks::end_preview_when_hidden(world, ctx.cumulative_frame_nr());
        crate::xr_simulator::xr_simulator_panel(&ctx, world);
        // Input-map writes made on the Project Settings page become undo entries (ADR-2771).
        for command in std::mem::take(&mut self.settings.input_undo) {
            crate::project_settings_ext::push_input_undo(world, command);
        }
        // Physics layer matrix writes, likewise (ADR-7233).
        for command in std::mem::take(&mut self.settings.physics_undo) {
            crate::physics_matrix::push_matrix_undo(world, command);
        }
        // Content-chunk settings saves (Regia.toml), likewise (CT-26).
        for command in std::mem::take(&mut self.settings.manifest_undo) {
            crate::closure_settings::push_manifest_undo(world, command);
        }
        // The dock may have been rearranged this frame; agents and the menus read the snapshot.
        self.publish_layout(world);
        self.views.publish(world);
        // Tool windows open in a cascade inside the dock area, below the toolbars (ADR-8589).
        crate::tool_windows::set_work_area(&ctx, dock_rect);
        self.modals(&ctx, world);
        // The brand looks' 2 px focus ring around a focused text field (ADR-8585), on top of it.
        crate::theme::paint_focus_ring(&ctx);
        if let Some(mut observation) = world.get_resource_mut::<crate::editor_layout::LayoutObservation>() {
            observation.finish(&ctx);
        }
        // The Material panel's edits of this frame join the shared history (P-322).
        crate::material_editor::sync_shared_history(world, &mut self.material);
        // The panel may have changed this frame; the next agent call must see it.
        self.publish_material_status(world);
    }

    /// Brings the Kit tab forward when a kit is opening (re-adding it when it was closed), and
    /// the Clip tab when the kit viewer played a clip.
    fn kit_focus(&mut self, world: &mut World) {
        let Some(mut kit) = world.get_resource_mut::<crate::kit_view::KitView>() else {
            return;
        };
        let panel = if std::mem::take(&mut kit.focus_kit_panel) {
            Panel::Kit
        } else if std::mem::take(&mut kit.focus_clip_panel) {
            Panel::AnimClip
        } else {
            return;
        };
        // Through `open_panel`: a maximized other tab is restored first, so the tab is not added
        // to the one-tab dock a restore drops (review sprint-v06-rev-late-k #10).
        self.open_panel(panel);
    }

    /// Keeps a tab for every panel an active extension registered and closes tabs whose panel
    /// went away (a disabled or reloaded extension leaves no dead tab).
    fn sync_extension_tabs(&mut self, world: &World) {
        let (wanted, gone) = {
            // The dock a maximized tab replaced: the one-tab dock never holds extension tabs.
            let open: Vec<&'static str> = self
                .views
                .base_dock(&self.dock)
                .iter_all_tabs()
                .filter_map(|(_, tab)| match tab {
                    Panel::ExtensionPanel(id, _) => Some(*id),
                    _ => None,
                })
                .collect();
            crate::extension::panel::tab_changes(world, &open)
        };
        self.apply_extension_tabs(wanted, gone);
    }

    /// Adds the `wanted` extension tabs and removes the `gone` ones, in the dock a maximized tab
    /// replaced when one is maximized (so the maximized view keeps its one tab and a restore keeps
    /// the change); a maximized extension panel whose extension went away is restored first.
    fn apply_extension_tabs(&mut self, wanted: Vec<(&'static str, &'static str)>, gone: Vec<&'static str>) {
        if let Some(Panel::ExtensionPanel(shown, _)) = self.views.maximized()
            && gone.contains(&shown)
        {
            self.views.restore(&mut self.dock);
        }
        let dock = self.views.base_dock_mut(&mut self.dock);
        for (id, title) in wanted {
            dock.push_to_focused_leaf(Panel::ExtensionPanel(id, title));
        }
        for id in gone {
            let tab = dock
                .iter_all_tabs()
                .find(|(_, tab)| matches!(tab, Panel::ExtensionPanel(open, _) if *open == id))
                .map(|(_, tab)| *tab);
            if let Some(path) = tab.and_then(|tab| dock.find_tab(&tab)) {
                dock.remove_tab(path);
            }
        }
    }

    /// Keeps a Game tab for every open PIE process view, closes tabs whose view went away, and
    /// brings a newly opened one forward (ADR-7831).
    fn sync_game_tabs(&mut self, world: &mut World) {
        let (wanted, focus) = match world.get_resource_mut::<crate::pie_process_view::GameViews>() {
            Some(mut views) => (views.roles(), views.focus.take()),
            None => (Vec::new(), None),
        };
        let open: Vec<crate::pie_process::ProcessRole> = self
            .dock
            .iter_all_tabs()
            .filter_map(|(_, tab)| match tab {
                Panel::GameView(role) => Some(*role),
                _ => None,
            })
            .collect();
        for role in &wanted {
            if !open.contains(role) {
                self.dock.push_to_focused_leaf(Panel::GameView(*role));
            }
        }
        for role in open.iter().filter(|r| !wanted.contains(r)) {
            if let Some(path) = self.dock.find_tab(&Panel::GameView(*role)) {
                self.dock.remove_tab(path);
            }
        }
        if let Some(role) = focus
            && let Some(path) = self.dock.find_tab(&Panel::GameView(role))
        {
            let _ = self.dock.set_active_tab(path);
        }
    }

    /// `editor.tool_panel` asked for the Tools tab: focus it (re-adding it when it was closed).
    fn tool_focus(&mut self, world: &mut World) {
        let Some(mut host) = world.get_resource_mut::<crate::tools::ToolHost>() else {
            return;
        };
        if !std::mem::take(&mut host.focus_panel) {
            return;
        }
        self.open_panel(Panel::Tool);
    }

    /// The 2D mode turned on without a map: focus the 2D tab (re-adding it when it was closed).
    fn mode2d_focus(&mut self, world: &mut World) {
        let Some(mut mode) = world.get_resource_mut::<crate::mode2d::Mode2d>() else {
            return;
        };
        if !std::mem::take(&mut mode.focus_panel) {
            return;
        }
        self.open_panel(Panel::Mode2d);
    }

    /// Opens a kit folder from File ▸ Open kit… or the project browser.
    fn open_kit(&mut self, world: &mut World, dir: &std::path::Path) -> String {
        let result = crate::kit_view::open_kit_in_world(world, dir);
        world
            .get_resource_or_insert_with(crate::kit_view::KitView::default)
            .focus_kit_panel = true;
        match result {
            Ok(kit) if kit.warnings.is_empty() => format!("Opened kit {}", kit.manifest.kit_id),
            Ok(kit) => format!(
                "Opened kit {} with {} warning(s)",
                kit.manifest.kit_id,
                kit.warnings.len()
            ),
            Err(err) => format!("Kit not opened: {err}"),
        }
    }

    /// Copies an agent-opened project onto the content browser and reloads crash recovery.
    fn adopt_project(&mut self, world: &World) {
        let Some(root) = world
            .get_resource::<crate::agent_ext::EditorProject>()
            .and_then(|project| project.root.clone())
        else {
            return;
        };
        if self.content.project_root.as_deref() == Some(root.as_path()) {
            return;
        }
        self.note_project_opened(root);
    }

    /// The browser just opened `root` (or the agent did, and the shell is catching up).
    fn note_project_opened(&mut self, root: std::path::PathBuf) {
        self.recovery = crate::project::recovery_for(&root);
        self.recovery_docs = crate::doc_autosave::doc_recovery_candidates(&root);
        self.offers_changed = true;
        self.content.project_root = Some(root.clone());
        self.toast = format!("opened {}", root.display());
    }

    /// Recent projects, Blank template, Open folder (`specs/19` § 6).
    fn draw_project_browser(&mut self, ui: &mut egui::Ui, world: &mut World) {
        // Kits open without a project (an outside modder viewing Tincal exports has none).
        let mut kit_dir = None;
        egui::Panel::bottom("project-browser-kit").show(ui, |ui| {
            ui.horizontal(|ui| {
                if crate::agent_parity::action_button(ui, &crate::agent_parity::FILE_OPEN_KIT).clicked() {
                    kit_dir = crate::file_dialog::FileDialog::new().pick_folder();
                }
                ui.weak("View an exported vehicle, prop or character kit.");
            });
        });
        if let Some(dir) = kit_dir {
            self.browser.status = self.open_kit(world, &dir);
            return;
        }
        let action = crate::project::browser_panel(ui, &mut self.browser);
        let opened = match action {
            Some(crate::project::BrowserAction::Open(path)) => crate::project::open_project(world, &path),
            Some(crate::project::BrowserAction::Create) => {
                let name = self.browser.name.clone();
                let parent = std::path::PathBuf::from(&self.browser.parent);
                // The browser's template picker; `editor.new_project` and `regia new` take the
                // same ids.
                let template = self.browser.template.clone();
                crate::project::create_and_open(world, &name, &parent, &template)
            }
            None => return,
        };
        match opened {
            Ok(opened) => {
                self.browser.status = opened.warning.unwrap_or_default();
                self.note_project_opened(opened.root);
            }
            Err(err) => self.browser.status = err,
        }
    }

    /// Crash-recovery and unsaved-changes modals (`specs/19` § 7), drawn over the dock.
    fn modals(&mut self, ctx: &egui::Context, world: &mut World) {
        crate::creators::script_wizard_window(ctx, world);
        crate::level_templates::dialog_window(ctx, world);
        if let Some(created) = world
            .get_resource_mut::<crate::level_templates::NewLevelDialog>()
            .and_then(|mut dialog| dialog.created.take())
        {
            self.toast = created;
        }
        crate::dcc_feed::accept_prompts(ctx, world);
        // The Profiler window (View → Profiler, ADR-2323) floats over the dock.
        crate::profiler_panel::window(ctx, world);
        crate::frame_debugger::window(ctx, world);
        // The Diagnostics window (View → Diagnostics, ADR-2771): GPU tab and trace timeline.
        crate::diagnostics_panel::window(ctx, world);
        // The Motion database editor window (spec 52 § 6, ADR-7740) floats over the dock.
        crate::mm_db_editor::panel::window(ctx, world);
        // The Rendering Debug window (View → Rendering Debug, ADR-7770).
        crate::render_debug_panel::window(ctx, world);
        // The Asset Jobs window (View → Asset Jobs, ADR-7788); an error click filters the Output Log.
        crate::asset_jobs_panel::window(ctx, world);
        if let Some(focus) = world.remove_resource::<crate::asset_jobs_panel::LogFocus>() {
            self.log.contains = focus.0;
        }
        // The prefab Overrides window (Prefab: Overrides…, the Outliner's dot, the Inspector; ADR-7790).
        crate::prefab_overrides_panel::window(ctx, world);
        // The Revision Control window (View → Revision Control, ADR-7837).
        crate::vcs::panel::window(ctx, world);
        crate::vcs::locks::window(ctx, world);
        // File ▸ Version History (ADR-7928).
        crate::version_history::window(ctx, world);
        let report = self.pending_reports.last().cloned();
        // Every level autosave and document a crashed session left behind (ADR-9770).
        let rows = crate::doc_autosave::RecoveryOffers {
            project_root: self.project_root(),
            levels: self.recovery.clone(),
            docs: self.recovery_docs.clone(),
            revision: 0,
        }
        .rows();
        if !crate::safe_mode::enabled(world) && (!rows.is_empty() || report.is_some()) {
            let shown = self.shown_report.clone();
            let mut choice = crate::crash_reports::RecoveryChoice::default();
            let mut list_action = None;
            egui::Window::new("Crash recovery")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    choice = crate::crash_reports::recovery_offer_ui(
                        ui,
                        &crate::crash_reports::RecoveryOffer {
                            autosave: None,
                            original: None,
                            report_line: report.as_ref().map(|item| item.first_line.as_str()),
                            report_path: report.as_ref().map(|item| item.path.as_path()),
                            shown_text: shown.as_deref(),
                        },
                    );
                    if !rows.is_empty() {
                        ui.separator();
                        list_action = crate::doc_autosave::recovery_list_ui(ui, &rows);
                    }
                });
            if let Some(action) = list_action {
                self.apply_recovery_action(world, &action);
            }
            if choice.dismiss {
                self.pending_reports.clear();
                self.shown_report = None;
            }
            if choice.view
                && let Some(report) = report.as_ref()
            {
                match crate::crash_reports::read_text(&report.path) {
                    Ok(text) => self.shown_report = Some(text),
                    Err(err) => self.toast = format!("could not read the crash report: {err}"),
                }
            }
        }
        // The Save all dialog (ADR-7822): File ▸ Save all, and the close's unsaved-changes prompt
        // (one list with checkboxes in place of the old per-document prompt).
        if self.close_prompt {
            self.close_prompt = false;
            crate::save_all_dialog::open_dialog(world, crate::save_all_dialog::SavePurpose::Close);
        }
        let extra = self.sync_save_all_rows(world);
        crate::save_all_dialog::dialog_window(ctx, world, &extra);
        let decision = world
            .get_resource_mut::<crate::save_all_dialog::SaveAllDialog>()
            .and_then(|mut dialog| dialog.take_decision());
        if let Some((purpose, decision)) = decision {
            self.apply_save_all_decision(world, &purpose, decision);
        }
    }

    /// The prefab-edit banner (`specs/19` § 2 "Prefab editor (isolated scene)"): shows the file,
    /// its status line, and the exits.
    fn prefab_mode_banner(&mut self, ui: &mut egui::Ui, world: &mut World) {
        let mode = world
            .get_resource::<crate::prefab_edit::PrefabEditMode>()
            .cloned();
        let Some(mode) = mode.filter(|mode| mode.editing().is_some()) else {
            return;
        };
        let path = mode.editing().expect("checked").display().to_string();
        egui::Panel::top("editor-prefab-mode").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(egui::Color32::from_rgb(200, 160, 60), "⬛ prefab edit mode");
                ui.weak(path);
                if !mode.toast.is_empty() {
                    ui.weak(format!("· {}", mode.toast));
                }
                crate::budget::prefab_budget_ui(ui, world);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if crate::agent_parity::action_button(ui, &crate::agent_parity::PREFAB_SAVE).clicked() {
                        match crate::prefab_edit::save_prefab(world) {
                            Ok(updated) => {
                                self.toast = format!("prefab saved · {updated} instance(s) updated");
                            }
                            Err(err) => self.toast = err,
                        }
                    }
                    if crate::agent_parity::action_button(ui, &crate::agent_parity::PREFAB_SAVE_CLOSE)
                        .clicked()
                    {
                        match crate::prefab_edit::exit_prefab_edit(world, true) {
                            Ok(()) => self.toast = "back to the level".into(),
                            Err(err) => self.toast = err,
                        }
                    }
                    if crate::agent_parity::action_button(ui, &crate::agent_parity::PREFAB_DISCARD_CLOSE)
                        .clicked()
                    {
                        match crate::prefab_edit::exit_prefab_edit(world, false) {
                            Ok(()) => self.toast = "prefab edits discarded".into(),
                            Err(err) => self.toast = err,
                        }
                    }
                    // Open graph / New graph for this prefab (P-165, ADR-8566).
                    if let Some(status) = crate::prefab_logic::banner_buttons(ui, world) {
                        self.toast = status;
                    }
                });
            });
        });
    }

    /// The Play / Pause / Stop toolbar (`specs/19` § 4). Requests queue into [`PieRequests`];
    /// the editor loop applies them and the status bar shows the result.
    fn play_toolbar(&mut self, ui: &mut egui::Ui, world: &mut World) {
        let status = world
            .get_resource_or_insert_with::<crate::pie::PieStatus>(Default::default)
            .clone();
        // The play-mode tint (ADR-7829): the viewed role's theme color while a session runs.
        let tint = crate::pie_controls::play_tint(world);
        egui::Panel::top("editor-play").show(ui, |ui| {
            // A named toolbar for screen readers (ADR-7746).
            crate::a11y_bridge::region(
                ui,
                egui::accesskit::Role::Toolbar,
                crate::a11y_bridge::PLAY_TOOLBAR,
            );
            if let Some(color) = tint {
                crate::pie_controls::paint_toolbar_tint(ui, color);
            }
            // Wrapped: at a large text scale the controls and the status continue on the next
            // row instead of running off the window's right edge; past 15 % of the window's
            // height (at least one row) the rows scroll, so a small window keeps its dock
            // (ADR-8679).
            let max_rows = (ui.ctx().content_rect().height() * 0.15)
                .max(ui.spacing().interact_size.y + 2.0 * ui.spacing().item_spacing.y);
            egui::ScrollArea::vertical()
                .id_salt("play-toolbar-rows")
                .max_height(max_rows)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        let paused = status.mode == Some(crate::pie::PlayMode::Paused);
                        let playing = status.mode == Some(crate::pie::PlayMode::Playing);
                        let active = playing || paused;
                        let queue = |world: &mut World, request: crate::pie::PieRequest| {
                            world
                                .get_resource_or_insert_with::<crate::pie::PieRequests>(Default::default)
                                .push(request);
                        };
                        // Play, Simulate, Pause or Resume, Stop, Step (ADR-9810 E-1, ADR-7381).
                        if let Some(request) = play_buttons(ui, status.mode) {
                            queue(world, request);
                        }
                        // Start option, speed and mute (ADR-7829); the network view switch with client worlds.
                        crate::pie_controls::toolbar_controls(ui, world, &status);
                        crate::pie_worlds::view_switch(ui, world);
                        crate::loc_panel::preview::preview_locale_picker(ui, world);
                        // The separate play window (ADR-7272), where the host has a window system.
                        if world.contains_resource::<crate::play_window::PlayWindowSupport>()
                            && ui
                                .button("\u{1f5d7} Play in window")
                                .on_hover_text(
                                    "Play with the game camera in a separate window; closing it stops play \
                             (Shift+F1 frees the mouse)",
                                )
                                .clicked()
                        {
                            crate::play_window::queue_play_in_window(world, ui.ctx().cumulative_frame_nr());
                        }
                        if world.contains_resource::<crate::play_window::PlayWindowSupport>() {
                            play_resolution_menu(ui, world);
                            // Device simulation in the play window (ADR-7829).
                            crate::play_device::device_menu(ui, world);
                        }
                        // The most used settings in one menu, each its home control's op (ADR-7754).
                        crate::shell_status::quick_settings_menu(ui, world);
                        if ui
                            .add_enabled(active, egui::Button::new("⤓ Keep changes"))
                            .on_hover_text(
                                "Copy the selected entities' play state back to the level (one undo step)",
                            )
                            .clicked()
                        {
                            queue(world, crate::pie::PieRequest::KeepSimulationChanges(None));
                        }
                        // Toolbar actions extensions register (ADR-9850).
                        if let Some(message) = crate::extension::actions::toolbar(ui, world) {
                            self.toast = message;
                        }
                        match status.mode {
                            Some(crate::pie::PlayMode::Playing) => {
                                ui.strong(format!("▶ playing · frame {}", status.frames));
                                if let Some(game) = &status.game {
                                    ui.weak(format!("· game {game}"));
                                }
                            }
                            Some(crate::pie::PlayMode::Paused) => {
                                ui.strong(format!("⏸ paused · frame {}", status.frames));
                            }
                            None => {
                                ui.weak("play stopped — Alt+P to play");
                            }
                        }
                        if let Some(line) = &status.xr {
                            ui.weak(format!("· {line}"));
                        }
                        if let Some(panic) = &status.panic {
                            ui.colored_label(
                                crate::theme::ui_tokens(ui.ctx()).accents.error_text,
                                format!("· last play ended by: {panic}"),
                            );
                        }
                        ui.separator();
                        // Game code (specs/19 § 5): Compile + idle/compiling/error/reloaded.
                        let code = world
                            .get_resource::<crate::code_reload::CodeReloadStatus>()
                            .cloned()
                            .unwrap_or_default();
                        let compiling =
                            matches!(code.state, Some(crate::hot_reload::CompileState::Compiling(_)));
                        let can_compile = code.game.is_some() && !compiling;
                        if crate::agent_parity::action_enabled(
                            ui,
                            can_compile,
                            &crate::agent_parity::PLAY_COMPILE,
                            None,
                        )
                        .on_hover_text("Rebuild and hot-reload the game code (Ctrl+Alt+F11)")
                        .clicked()
                        {
                            world.insert_resource(crate::code_reload::CompileRequest(true));
                        }
                        let label = code.label();
                        match code.state {
                            Some(crate::hot_reload::CompileState::Error(_)) => {
                                ui.colored_label(crate::theme::ui_tokens(ui.ctx()).accents.error_text, label);
                            }
                            Some(crate::hot_reload::CompileState::Compiling(_)) => {
                                ui.colored_label(crate::theme::ui_tokens(ui.ctx()).accents.unsaved, label);
                                // The build's Cancel (ADR-9770; `editor.cancel_task { id: "game_build" }`).
                                if crate::tasks::cancel_button(ui, world, crate::tasks::GAME_BUILD) {
                                    self.toast = "cancelling the game build".into();
                                }
                            }
                            _ => {
                                ui.weak(label);
                            }
                        }
                        // The workspace switcher at the toolbar's right end (ADR-7753).
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            crate::workspaces::workspace_switcher(ui, world);
                        });
                    });
                });
            crate::workspaces::mode_toolbar(ui, world);
        });
    }

    /// Switches the level at once when nothing is unsaved; otherwise opens the Save all dialog,
    /// which switches after **Save selected** or **Don't save** (ADR-7822; it replaced the old
    /// click-twice-to-discard).
    fn switch_level_or_ask(
        &mut self,
        world: &mut World,
        unsaved: bool,
        switch: crate::save_all_dialog::LevelSwitch,
    ) -> String {
        if unsaved {
            crate::save_all_dialog::open_dialog(
                world,
                crate::save_all_dialog::SavePurpose::LevelSwitch(switch),
            );
            return "Unsaved changes: choose what to save".into();
        }
        Self::switch_level(world, &switch)
    }

    /// Replaces the open level (File ▸ New, Open, Recent) and answers the toast line.
    fn switch_level(world: &mut World, switch: &crate::save_all_dialog::LevelSwitch) -> String {
        match switch {
            crate::save_all_dialog::LevelSwitch::New => {
                let open = level_io::new_level(world);
                world.insert_resource(open);
                level_io::reset_edit_state(world);
                "New level".into()
            }
            crate::save_all_dialog::LevelSwitch::Open(path) => match level_io::open_level_file(world, path) {
                Ok(open) => {
                    world.insert_resource(open);
                    level_io::reset_edit_state(world);
                    crate::viewport_ops::note_recent_level(world);
                    format!("Opened {}", path.display())
                }
                Err(err) => format!("Open failed: {err}"),
            },
        }
    }

    /// The menu bar: File, Create, Edit, View, Tools, Play and Help (ADR-7475).
    fn menu_bar(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, world: &mut World) {
        // File ▸ Recent follows the open level (idempotent, so the second UI pass is harmless).
        crate::viewport_ops::note_recent_level(world);
        egui::Panel::top("editor-menu").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong("Regia Engine");
                let mut toast = String::new();
                // The File and Edit items shared with `editor.menu_invoke` (ADR-7204).
                let menu = |world: &mut World, path: &str| match crate::viewport_ops::invoke_menu(world, path)
                {
                    Ok(_) => String::new(),
                    Err(err) => format!("{path}: {err}"),
                };
                crate::agent_parity::menu(ui, "File", |ui| {
                    let prefab_mode = world
                        .get_resource::<crate::prefab_edit::PrefabEditMode>()
                        .is_some_and(|mode| mode.editing().is_some());
                    if prefab_mode {
                        ui.weak("close the prefab editor to switch levels");
                    }
                    let unsaved = world.get_resource::<UndoStack>().is_some_and(UndoStack::is_dirty);
                    if !prefab_mode
                        && crate::agent_parity::action_menu_item(ui, &crate::agent_parity::FILE_NEW_LEVEL)
                            .clicked()
                    {
                        // The template dialog (`level_templates`): Empty is the old behavior, and the
                        // dialog asks before discarding unsaved edits.
                        crate::level_templates::open_dialog(world);
                        ui.close();
                    }
                    if !prefab_mode
                        && crate::agent_parity::action_menu_item(ui, &crate::agent_parity::FILE_OPEN_LEVEL)
                            .clicked()
                    {
                        if let Some(path) = crate::file_dialog::FileDialog::new()
                            .add_filter("Regia Engine level", &["ron"])
                            .pick_file()
                        {
                            toast = self.switch_level_or_ask(
                                world,
                                unsaved,
                                crate::save_all_dialog::LevelSwitch::Open(path),
                            );
                        }
                        ui.close();
                    }
                    // File ▸ New level variant…: a base level plus only its differences (ADR-7823).
                    if !prefab_mode
                        && crate::agent_parity::action_menu_item(
                            ui,
                            &crate::agent_parity::FILE_NEW_LEVEL_VARIANT,
                        )
                        .clicked()
                    {
                        if !crate::level_variant::confirm_new_variant(world, unsaved) {
                            toast =
                                "Unsaved changes: save first, or click New level variant again to discard"
                                    .into();
                        } else if let Some(text) = crate::level_variant::new_variant_from_dialogs(world) {
                            toast = text;
                        }
                        ui.close();
                    }
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::FILE_NEW_SCRIPT)
                        .clicked()
                    {
                        // The New script wizard (specs/19 § 5, ADR-7234).
                        world
                            .get_resource_or_insert_with(crate::creators::ScriptWizard::default)
                            .open = true;
                        ui.close();
                    }
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::FILE_OPEN_KIT)
                        .clicked()
                    {
                        if let Some(dir) = crate::file_dialog::FileDialog::new().pick_folder() {
                            toast = self.open_kit(world, &dir);
                        }
                        ui.close();
                    }
                    let dirty = world
                        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                        .is_dirty();
                    if crate::agent_parity::action_button_text(
                        ui,
                        &crate::agent_parity::FILE_SAVE_LEVEL,
                        format!("Save level{}", if dirty { " *" } else { "" }),
                    )
                    .clicked()
                    {
                        toast = self.save_level(world);
                        ui.close();
                    }
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::FILE_SAVE_LEVEL_AS)
                        .clicked()
                    {
                        // A prefab open in place: refused before the file dialog (ADR-7835).
                        if let Some(refusal) = crate::prefab_edit::level_save_refusal(world) {
                            toast = format!("Save refused: {refusal}");
                        } else if let Some(path) = crate::file_dialog::FileDialog::new()
                            .add_filter("Regia Engine level", &["ron"])
                            .set_file_name("untitled.level.ron")
                            .save_file()
                        {
                            let mut open = world
                                .get_resource::<OpenLevel>()
                                .cloned()
                                .unwrap_or_else(|| level_io::new_level(world));
                            if level_io::save_level_and_terrain(world, &open, &path).is_ok() {
                                if let Some(root) = self.project_root() {
                                    crate::autosave::clear_autosave_after_save(&root, &open, &path);
                                }
                                open.path = Some(path.clone());
                                world.insert_resource(open);
                                crate::terrain_tool::commit_terrain_save(world);
                                // Clean only once the tile map is written too (ADR-7412).
                                let tiles = crate::mode2d::save_dirty_tilemap_and_mark_saved(world);
                                toast = format!("Saved {}{tiles}", path.display());
                            } else {
                                toast = "Save failed".into();
                            }
                        }
                        ui.close();
                    }
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::FILE_SAVE_ALL)
                        .clicked()
                    {
                        // Every dirty document in one list with checkboxes (ADR-7822); the agent's
                        // `editor.save_all { dialog: true }` opens the same dialog.
                        crate::save_all_dialog::open_dialog(
                            world,
                            crate::save_all_dialog::SavePurpose::SaveAll,
                        );
                        ui.close();
                    }
                    let recent = world
                        .get_resource::<crate::viewport_ops::RecentLevels>()
                        .map(|r| r.paths.clone())
                        .unwrap_or_default();
                    ui.add_enabled_ui(!prefab_mode && !recent.is_empty(), |ui| {
                        crate::agent_parity::menu(ui, "Recent", |ui| {
                            for (i, path) in recent.iter().enumerate() {
                                if crate::agent_parity::action_button_text(
                                    ui,
                                    &crate::agent_parity::FILE_RECENT,
                                    path.display().to_string(),
                                )
                                .clicked()
                                {
                                    toast = if unsaved {
                                        self.switch_level_or_ask(
                                            world,
                                            unsaved,
                                            crate::save_all_dialog::LevelSwitch::Open(path.clone()),
                                        )
                                    } else {
                                        let failed = menu(world, &format!("File/Recent/{i}"));
                                        if failed.is_empty() {
                                            format!("Opened {}", path.display())
                                        } else {
                                            failed
                                        }
                                    };
                                    ui.close();
                                }
                            }
                        });
                    });
                    if let Some(message) = crate::export_glb::menu_entry(ui, world, self.project_root()) {
                        toast = message;
                    }
                    // File ▸ Package for platform… (ADR-7475): the dialog runs `regia package` as a job.
                    if crate::agent_parity::action_enabled(
                        ui,
                        self.project_root().is_some(),
                        &crate::agent_parity::FILE_PACKAGE,
                        None,
                    )
                    .clicked()
                    {
                        let root = self.project_root();
                        world
                            .get_resource_or_insert_with(crate::package_job::PackageDialog::default)
                            .open_for(root.as_deref());
                        ui.close();
                    }
                    // File ▸ Screenshot… (ADR-7768).
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::FILE_SCREENSHOT)
                        .clicked()
                    {
                        crate::screenshot::open_dialog(world);
                        ui.close();
                    }
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::FILE_CREATE_ENTITY)
                        .clicked()
                    {
                        match SpawnEntities::new_entity(
                            world,
                            regia_core::id::Guid::new_random(),
                            "New entity",
                            regia_scene::prelude::Transform::default(),
                        ) {
                            Ok(mut command) => {
                                command.apply(world);
                                world
                                    .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                                    .push(Box::new(command));
                                toast = "Entity created".into();
                            }
                            Err(err) => toast = format!("Create failed: {err}"),
                        }
                        ui.close();
                    }
                    // File ▸ Version history… and Duplicate project… (ADR-7928).
                    if let Some(message) = self.history_menu_entries(ui, world) {
                        toast = message;
                    }
                    ui.separator();
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::FILE_EXIT).clicked() {
                        // The window's close request: unsaved changes ask first (ADR-2179).
                        toast = menu(world, "File/Exit");
                        ui.close();
                    }
                });
                crate::agent_parity::menu(ui, "Create", |ui| {
                    if let Some(message) = crate::shapes::shape_menu(ui, world) {
                        toast = message;
                    }
                });
                crate::agent_parity::menu(ui, "Edit", |ui| {
                    // Read-only snapshot of the stack's labels/depths (the stack itself is
                    // moved out of the world when the buttons act — see `undo_world`).
                    let (can_undo, can_redo, undo_label, redo_label) = {
                        let undo = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
                        (
                            undo.undo_depth() > 0,
                            undo.redo_depth() > 0,
                            undo.next_undo()
                                .map_or_else(|| "Undo".to_string(), |d| format!("Undo — {d}")),
                            undo.next_redo()
                                .map_or_else(|| "Redo".to_string(), |d| format!("Redo — {d}")),
                        )
                    };
                    if crate::agent_parity::action_enabled(
                        ui,
                        can_undo,
                        &crate::agent_parity::EDIT_UNDO,
                        Some(undo_label),
                    )
                    .clicked()
                    {
                        crate::command::undo_world(world);
                        ui.close();
                    }
                    if crate::agent_parity::action_enabled(
                        ui,
                        can_redo,
                        &crate::agent_parity::EDIT_REDO,
                        Some(redo_label),
                    )
                    .clicked()
                    {
                        crate::command::redo_world(world);
                        ui.close();
                    }
                    ui.separator();
                    let any = world
                        .get_resource::<crate::selection::Selection>()
                        .is_some_and(|s| !s.is_empty());
                    let clip = world
                        .get_resource::<crate::viewport_ops::EditorClipboard>()
                        .is_some_and(|c| c.file.is_some());
                    for (action, path, enabled) in [
                        (&crate::agent_parity::EDIT_CUT, "Edit/Cut", any),
                        (&crate::agent_parity::EDIT_COPY, "Edit/Copy", any),
                        (&crate::agent_parity::EDIT_PASTE, "Edit/Paste", clip),
                        (
                            &crate::agent_parity::EDIT_PASTE_AS_CHILD,
                            "Edit/Paste as child",
                            clip && any,
                        ),
                        (
                            &crate::agent_parity::EDIT_PASTE_AT_ORIGINAL,
                            "Edit/Paste at original position",
                            clip,
                        ),
                        (&crate::agent_parity::EDIT_DUPLICATE, "Edit/Duplicate", any),
                        (&crate::agent_parity::EDIT_DELETE, "Edit/Delete", any),
                        (&crate::agent_parity::EDIT_SELECT_ALL, "Edit/Select all", true),
                    ] {
                        if crate::agent_parity::action_enabled(ui, enabled, action, None).clicked() {
                            toast = menu(world, path);
                            ui.close();
                        }
                    }
                    // Edit ▸ Select ▸ select similar (ADR-7769).
                    crate::agent_parity::menu(ui, "Select", |ui| {
                        if let Some(status) = crate::selection::similar_menu_items(ui, world) {
                            toast = status;
                        }
                    });
                    ui.separator();
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::EDIT_PREFERENCES)
                        .clicked()
                    {
                        self.open_panel(Panel::Preferences);
                        ui.close();
                    }
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::EDIT_PROJECT_SETTINGS)
                        .clicked()
                    {
                        self.open_panel(Panel::ProjectSettings);
                        ui.close();
                    }
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::EDIT_WORLD_SETTINGS)
                        .clicked()
                    {
                        self.open_panel(Panel::WorldSettings);
                        ui.close();
                    }
                    // The selected prefab instance's Overrides window and Edit in place (ADR-7790).
                    ui.separator();
                    for action in [
                        &crate::agent_parity::EDIT_PREFAB_OVERRIDES,
                        &crate::agent_parity::EDIT_PREFAB_IN_PLACE,
                    ] {
                        if crate::agent_parity::action_menu_item(ui, action).clicked() {
                            if let crate::agent_parity::ActionPath::Command(id) = action.path
                                && let Some(status) = crate::command_palette::run(world, id)
                            {
                                self.toast = status;
                            }
                            ui.close();
                        }
                    }
                });
                crate::agent_parity::menu(ui, "View", |ui| {
                    let mut settings = world
                        .get_resource_or_insert_with::<ViewportSettings>(ViewportSettings::default)
                        .clone();
                    crate::agent_parity::action_toggle(
                        ui,
                        &crate::agent_parity::VIEW_GRID,
                        &mut settings.grid,
                    );
                    crate::agent_parity::action_toggle(
                        ui,
                        &crate::agent_parity::VIEW_LABELS,
                        &mut settings.labels,
                    );
                    crate::agent_parity::action_toggle(
                        ui,
                        &crate::agent_parity::VIEW_REALTIME,
                        &mut settings.realtime,
                    );
                    world.insert_resource(settings);
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::VIEW_FRAME_DEBUGGER)
                        .clicked()
                    {
                        crate::frame_debugger::toggle_ui(world, ui.ctx().cumulative_frame_nr());
                        ui.close();
                    }
                    let mut profiler = world
                        .get_resource_or_insert_with::<crate::profiler_panel::ProfilerPanel>(Default::default)
                        .clone();
                    if crate::agent_parity::action_toggle(
                        ui,
                        &crate::agent_parity::VIEW_PROFILER,
                        &mut profiler.open,
                    )
                    .changed()
                    {
                        world.insert_resource(profiler);
                    }
                    let mut diagnostics = world
                        .get_resource_or_insert_with::<crate::diagnostics_panel::DiagnosticsPanel>(
                            Default::default,
                        )
                        .clone();
                    if crate::agent_parity::action_toggle(
                        ui,
                        &crate::agent_parity::VIEW_DIAGNOSTICS,
                        &mut diagnostics.open,
                    )
                    .changed()
                    {
                        world.insert_resource(diagnostics);
                    }
                    let mut rendering_debug = world
                        .get_resource_or_insert_with(crate::render_debug_panel::RenderDebugPanel::default)
                        .clone();
                    if crate::agent_parity::action_toggle(
                        ui,
                        &crate::agent_parity::VIEW_RENDERING_DEBUG,
                        &mut rendering_debug.open,
                    )
                    .changed()
                    {
                        world.insert_resource(rendering_debug);
                    }
                    // View → Asset Jobs (spec addendum § 8.9, ADR-7788).
                    let mut asset_jobs = world
                        .get_resource_or_insert_with::<crate::asset_jobs_panel::AssetJobsPanel>(
                            Default::default,
                        )
                        .clone();
                    if crate::agent_parity::action_toggle(
                        ui,
                        &crate::agent_parity::VIEW_ASSET_JOBS,
                        &mut asset_jobs.open,
                    )
                    .changed()
                    {
                        world.insert_resource(asset_jobs);
                    }
                    let mut revision_open = world
                        .get_resource::<crate::vcs::panel::VcsPanel>()
                        .is_some_and(|p| p.open);
                    if crate::agent_parity::action_toggle(
                        ui,
                        &crate::agent_parity::VIEW_REVISION_CONTROL,
                        &mut revision_open,
                    )
                    .changed()
                    {
                        crate::vcs::panel::toggle(world);
                    }
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::VIEW_FRAME_SELECTION)
                        .clicked()
                    {
                        crate::command_palette::frame_selection(world);
                        ui.close();
                    }
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::VIEW_RESET_ZOOM)
                        .clicked()
                    {
                        toast = crate::command_palette::run(world, "view.reset_zoom").unwrap_or_default();
                        ui.close();
                    }
                    // View ▸ Move camera to selection / Move selection to camera: the palette
                    // commands on the main viewport's camera (ADR-7755).
                    for action in [
                        &crate::agent_parity::VIEW_CAMERA_TO_SELECTION,
                        &crate::agent_parity::VIEW_SELECTION_TO_CAMERA,
                    ] {
                        if crate::agent_parity::action_menu_item(ui, action).clicked() {
                            if let crate::agent_parity::ActionPath::Command(id) = action.path {
                                toast = crate::command_palette::run(world, id).unwrap_or_default();
                            }
                            ui.close();
                        }
                    }
                    ui.separator();
                    ui.weak(format!("Layout: {}", self.layout_source.as_str()));
                    if crate::agent_parity::action_enabled(
                        ui,
                        self.project_root().is_some(),
                        &crate::agent_parity::VIEW_LAYOUT_SAVE,
                        None,
                    )
                    .clicked()
                    {
                        toast = self.save_layout(world, false);
                        ui.close();
                    }
                    if crate::agent_parity::action_menu_item(
                        ui,
                        &crate::agent_parity::VIEW_LAYOUT_SAVE_DEFAULT,
                    )
                    .clicked()
                    {
                        toast = self.save_layout(world, true);
                        ui.close();
                    }
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::VIEW_LAYOUT_RESET)
                        .clicked()
                    {
                        self.set_layout(
                            crate::editor_layout::default_dock(),
                            crate::editor_layout::LayoutSource::Default,
                        );
                        toast = "Layout reset to the built-in one".into();
                        ui.close();
                    }
                    // Workspaces, named layouts, maximize, full screen, panels and feature profiles
                    // (ADR-7753).
                    ui.separator();
                    if let Some(line) = crate::workspaces::view_menu(ui, world, &mut self.layout_naming) {
                        toast = line;
                    }
                    crate::agent_parity::menu(ui, crate::agent_parity::VIEW_FEATURE_PROFILE.label, |ui| {
                        if let Some(line) = crate::feature_profiles::profile_menu(ui, world) {
                            toast = line;
                        }
                    });
                    // View ▸ Bulk Edit (ADR-7787): the selection in the grid, or the empty panel.
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::VIEW_BULK_EDIT)
                        .clicked()
                    {
                        let selected = world
                            .get_resource::<Selection>()
                            .map(|s| s.entities().to_vec())
                            .unwrap_or_default();
                        if selected.is_empty() {
                            crate::editor_layout::LayoutRequests::push(
                                world,
                                crate::editor_layout::LayoutRequest::Open(Panel::BulkEdit),
                            );
                        } else {
                            toast = crate::bulk_edit_panel::edit_entities_in_grid(world, &selected);
                        }
                        ui.close();
                    }
                    // View ▸ Color Vision: the chrome and the viewport (ADR-7838).
                    ui.separator();
                    crate::chrome_color_vision::view_menu(ui, world);
                });
                // Tools, Play and Help (spec 19 § 2, ADR-7475; `editor.menu_list` lists every item).
                crate::agent_parity::menu(ui, "Tools", |ui| {
                    if let Some(message) = crate::menus::tools_menu(ui, world) {
                        toast = message;
                    }
                });
                // Sequence ▸ Render movie… (ADR-7726).
                crate::agent_parity::menu(ui, "Sequence", |ui| {
                    if let Some(message) = crate::menus::sequence_menu(ui, world) {
                        toast = message;
                    }
                });
                crate::agent_parity::menu(ui, "Play", |ui| {
                    if let Some(request) = crate::menus::play_menu(ui, world) {
                        world
                            .get_resource_or_insert_with::<crate::pie::PieRequests>(Default::default)
                            .push(request);
                    }
                });
                crate::agent_parity::menu(ui, "Help", |ui| {
                    if let Some(message) = crate::menus::help_menu(ui, world) {
                        toast = message;
                    }
                });
                crate::agent_parity::menu(ui, "Window", |ui| {
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::WINDOW_UNDO_HISTORY)
                        .clicked()
                        && crate::history_panel::open_once(world, ui.ctx().cumulative_frame_nr())
                    {
                        self.open_panel(Panel::UndoHistory);
                        ui.close();
                    }
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::WINDOW_AGENT).clicked()
                    {
                        self.open_panel(Panel::Agent);
                        ui.close();
                    }
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::WINDOW_NOTES).clicked()
                    {
                        self.open_panel(Panel::Notes);
                        ui.close();
                    }
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::WINDOW_ACTIVITY)
                        .clicked()
                    {
                        self.open_panel(Panel::Activity);
                        ui.close();
                    }
                    // Window ▸ New Outliner / Content Browser / Inspector (P-003, ADR-8105 (a)):
                    // the lowest free instance, with fresh state, beside its panel. Once per frame:
                    // egui may run the menu twice.
                    ui.separator();
                    for (action, kind) in [
                        (
                            &crate::agent_parity::WINDOW_NEW_OUTLINER,
                            InstancedPanel::Outliner,
                        ),
                        (&crate::agent_parity::WINDOW_NEW_CONTENT, InstancedPanel::Content),
                        (
                            &crate::agent_parity::WINDOW_NEW_INSPECTOR,
                            InstancedPanel::Inspector,
                        ),
                    ] {
                        let free = self.free_instance(kind);
                        let response = ui.add_enabled_ui(free.is_some(), |ui| {
                            crate::agent_parity::action_menu_item(ui, action)
                        });
                        let response = response.inner.on_disabled_hover_text(format!(
                            "{} instances of the {} are open (the most)",
                            MAX_PANEL_INSTANCE + 1,
                            kind.base().title()
                        ));
                        let frame = ui.ctx().cumulative_frame_nr();
                        let once = egui::Id::new(("window-new-instance", kind));
                        if response.clicked()
                            && ui.ctx().data(|d| d.get_temp::<u64>(once)) != Some(frame)
                            && let Some(panel) = free
                        {
                            ui.ctx().data_mut(|d| d.insert_temp(once, frame));
                            self.open_new_instance(panel);
                            ui.close();
                        }
                    }
                    // One Window menu for 3255's Undo History, 3266's Agent, 3259's Notes and Activity
                    // and 3261's build tools.
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::SIZE_REPORT_OPEN)
                        .clicked()
                    {
                        world
                            .get_resource_or_insert_with(crate::size_report_panel::SizeReportPanel::default)
                            .open = true;
                        ui.close();
                    }
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::PLATFORMS_OPEN)
                        .clicked()
                    {
                        world
                            .get_resource_or_insert_with(crate::platforms_panel::PlatformsPanel::default)
                            .open = true;
                        ui.close();
                    }
                    if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::LAUNCH_PROFILES_OPEN)
                        .clicked()
                    {
                        world
                            .get_resource_or_insert_with(crate::launch_profiles::ProfilesPanel::default)
                            .open = true;
                        ui.close();
                    }
                    // Window ▸ Utility Panels (ADR-8189).
                    if let Some(message) = crate::utility_panel::window_menu(ui, world) {
                        toast = message;
                    }
                });
                if let Some(message) = crate::menus::windows(ctx, world) {
                    toast = message;
                }
                // Actions extensions register (ADR-9850); the menu shows only when there are any.
                if let Some(message) = crate::extension::actions::menu(ui, world) {
                    toast = message;
                }
                if !toast.is_empty() {
                    self.toast = toast;
                }
            });
        });
    }

    /// File ▸ Version history… (opens the window) and Duplicate project… (a folder dialog, then
    /// `project::duplicate_project`). Returns the toast of a duplicate.
    fn history_menu_entries(&mut self, ui: &mut egui::Ui, world: &mut World) -> Option<String> {
        let root = self.project_root();
        let mut toast = None;
        if crate::agent_parity::action_enabled(
            ui,
            root.is_some(),
            &crate::agent_parity::FILE_VERSION_HISTORY,
            None,
        )
        .clicked()
        {
            world
                .get_resource_or_insert_with(crate::version_history::VersionHistoryPanel::default)
                .open = true;
            ui.close();
        }
        if crate::agent_parity::action_enabled(
            ui,
            root.is_some(),
            &crate::agent_parity::FILE_DUPLICATE_PROJECT,
            None,
        )
        .clicked()
        {
            if let Some(root) = &root {
                let name = root
                    .file_name()
                    .map_or_else(|| "project".to_string(), |n| n.to_string_lossy().into_owned());
                if let Some(dest) = crate::file_dialog::FileDialog::new()
                    .set_title("Duplicate project to a new folder")
                    .set_file_name(format!("{name} copy"))
                    .save_file()
                {
                    toast = Some(match crate::project::duplicate_project(root, &dest) {
                        Ok(report) => format!(
                            "Duplicated the project to {} ({} files)",
                            report.root.display(),
                            report.files
                        ),
                        Err(err) => format!("Duplicate failed: {}", err.message),
                    });
                }
            }
            ui.close();
        }
        toast
    }

    /// Saves the open level (ask for a path when it has none).
    fn save_level(&mut self, world: &mut World) -> String {
        // A prefab open in place: refused before a file dialog opens (ADR-7835).
        if let Some(refusal) = crate::prefab_edit::level_save_refusal(world) {
            return format!("Save refused: {refusal}");
        }
        let mut open = world
            .get_resource::<OpenLevel>()
            .cloned()
            .unwrap_or_else(|| level_io::new_level(world));
        let before = open.clone();
        if open.path.is_none() {
            match crate::file_dialog::FileDialog::new()
                .add_filter("Regia Engine level", &["ron"])
                .set_file_name("untitled.level.ron")
                .save_file()
            {
                Some(path) => open.path = Some(path),
                None => return "Save canceled".into(),
            }
        }
        let Some(path) = open.path.clone() else {
            return "Save canceled".into();
        };
        match level_io::save_level_and_terrain(world, &open, &path) {
            Ok(_) => {
                // A successful save supersedes the level's autosave, including the
                // `untitled` file when this was the first save (specs/19 § 7).
                if let Some(root) = self.project_root() {
                    crate::autosave::clear_autosave_after_save(&root, &before, &path);
                }
                world.insert_resource(open);
                crate::terrain_tool::commit_terrain_save(world);
                // Clean only once the tile map is written too (ADR-7412).
                let tiles = crate::mode2d::save_dirty_tilemap_and_mark_saved(world);
                format!("Saved {}{tiles}", path.display())
            }
            Err(err) => format!("Save failed: {err}"),
        }
    }

    /// The bottom status bar: toast, the clickable indicators (drawers, unsaved, jobs, trace,
    /// problems, revision control; ADR-7754), undo depths, selection, gizmo mode. Returns its
    /// rectangle (presses there do not close a drawer: its buttons toggle them).
    fn status_bar(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, world: &mut World) -> egui::Rect {
        let _ = ctx;
        // The unsaved documents the Unsaved indicator and `editor.status_bar` count.
        let unsaved = crate::shell_status::UnsavedDocuments(self.unsaved_documents(world));
        if world.get_resource::<crate::shell_status::UnsavedDocuments>() != Some(&unsaved) {
            world.insert_resource(unsaved);
        }
        let shell = self;
        let mut save_all = false;
        let rect = egui::Panel::bottom("editor-status").show(ui, |ui| {
            ui.horizontal(|ui| {
                let (undo_depth, redo_depth, dirty, selected, gizmo) = {
                    let undo = world.get_resource_or_insert_with::<UndoStack>(UndoStack::new);
                    let depths = (undo.undo_depth(), undo.redo_depth(), undo.is_dirty());
                    let selected = world
                        .get_resource_or_insert_with::<Selection>(Selection::default)
                        .len();
                    let gizmo = world
                        .get_resource_or_insert_with::<crate::viewport::GizmoState>(
                            crate::viewport::GizmoState::default,
                        )
                        .mode
                        .name();
                    (depths.0, depths.1, depths.2, selected, gizmo)
                };
                if !shell.toast.is_empty() {
                    ui.label(&shell.toast);
                    if ui.small_button("×").clicked() {
                        shell.toast.clear();
                    }
                    ui.separator();
                }
                // Live asset reload (specs/12 § 8): the latest "reloaded X in N ms" /
                // "cook error in Y: …".
                if let Some(status) = world.get_resource::<regia_asset::AssetWatchStatus>()
                    && let Some(last) = status.recent.back()
                {
                    let line = last.toast();
                    match last.outcome {
                        regia_asset::WatchOutcome::Reloaded => {
                            ui.weak(line);
                        }
                        regia_asset::WatchOutcome::Error(_) => {
                            ui.colored_label(crate::theme::ui_tokens(ui.ctx()).accents.error_text, line);
                        }
                    }
                    ui.separator();
                }
                if let Some(pending) = world
                    .get_resource::<crate::asset_watch::PendingLevelReload>()
                    .cloned()
                {
                    let name = pending
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    ui.colored_label(
                        crate::theme::ui_tokens(ui.ctx()).accents.unsaved,
                        format!("{name} changed on disk — discard your unsaved edits?"),
                    );
                    if ui.small_button("Reload from disk").clicked() {
                        crate::asset_watch::resolve_level_reload(world, true);
                    }
                    if ui.small_button("Keep mine").clicked() {
                        crate::asset_watch::resolve_level_reload(world, false);
                    }
                    ui.separator();
                }
                ui.weak(format!(
                    "undo {undo_depth} · redo {redo_depth} · selected {selected}"
                ));
                ui.weak(format!("· gizmo: {gizmo}"));
                if dirty {
                    ui.colored_label(
                        crate::theme::ui_tokens(ui.ctx()).accents.unsaved,
                        "· unsaved changes",
                    );
                }
                if !shell.has_device() {
                    ui.weak("· no GPU device (viewport cleared, gizmos active)");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    match crate::shell_status::indicators(ui, world) {
                        Some(crate::shell_status::StatusClick::SaveAll) => save_all = true,
                        Some(crate::shell_status::StatusClick::Toast(toast)) => shell.toast = toast,
                        None => {}
                    }
                });
                // The revision-control slot (spec 19 § 1.6, ADR-7837): branch and changed files.
                crate::vcs::status_bar_slot(ui, world);
            });
        });
        // The Unsaved indicator is File ▸ Save all (the menu item's action, ADR-7822): the same
        // dialog, one code path per action (ADR-7752 (3)).
        if save_all {
            crate::save_all_dialog::open_dialog(world, crate::save_all_dialog::SavePurpose::SaveAll);
        }
        rect.response.rect
    }

    /// Applies shell-level per-frame side effects (viewport drops from the content browser,
    /// the autosave tick).
    pub fn frame_effects(&mut self, ctx: &egui::Context, world: &mut World) {
        crate::collab::frame_tick(world, ctx);
        // Bookkeeping entities that appeared since the last frame (protocol, platform) join
        // the hidden set; user content is never affected (it always has name/guid/transform).
        crate::visibility::mark_internal_entities(world);
        // User entities created through paths that do not add scene identity (agent
        // `scene.spawn`) get it here, so saves and PIE never drop them.
        crate::visibility::ensure_scene_identity(world);
        // Terrain strokes: brush-thread batches into the terrain, finished strokes and agent
        // strokes onto the undo stack, the gizmo brush preview.
        crate::terrain_tool::frame_effects(world);
        // Procedural graphs: hot-reload poll, preview quiet period, visualization, tool re-runs.
        crate::procgen::tick(world);
        let playing = world
            .get_resource_or_insert_with::<crate::pie::PieStatus>(Default::default)
            .mode
            .is_some();
        let mut open = world
            .get_resource::<OpenLevel>()
            .cloned()
            .unwrap_or_else(|| level_io::new_level(world));
        // While playing, the panels show the play world; a drop would spawn into the edit world
        // where nobody sees it. Refuse it instead (`specs/19` § 4: editing during play edits the
        // play world; asset placement is an edit-world action).
        if playing {
            if egui::DragAndDrop::payload::<crate::dnd::ContentDrag>(ctx).is_some()
                && ctx.input(|i| i.pointer.any_released())
            {
                egui::DragAndDrop::clear_payload(ctx);
                self.toast = "stop play to place assets".into();
            }
        } else if let Some(toast) = handle_viewport_drop(ctx, world, &self.content, &mut open) {
            self.toast = toast;
        }
        world.insert_resource(open);
        // Files dropped from the OS (spec addendum § 8.7, ADR-7788).
        let root = self.content.project_root.clone();
        if let Some(toast) = crate::os_drop::handle_os_drops(ctx, world, root.as_deref(), playing) {
            self.toast = toast;
        }
        let mut selection = world
            .get_resource_or_insert_with::<Selection>(Selection::default)
            .clone();
        selection.retain_alive(world);
        world.insert_resource(selection);
        edit_shortcuts(ctx, world, playing);
        play_shortcuts(ctx, world, playing);
        // Auto key with the Sequence Curves panel closed: an edit that ended this frame (the
        // pointer release above sealed it) is keyed here, once (ADR-8730).
        crate::seq_tracks::auto_key_frame(world);

        // The autosave tick (`specs/19` § 7, ADR-9770): every `autosave_every`, the dirty level
        // and every asset editor's dirty documents are snapshotted here and written on the
        // autosave worker; a document saved since loses its autosave.
        self.sync_offers(world);
        if let Some(root) = self.project_root() {
            crate::doc_autosave::install(world);
            let interval = (self.autosave_every != std::time::Duration::MAX).then_some(self.autosave_every);
            let before = {
                let mut state = world.resource_mut::<crate::doc_autosave::AutosaveState>();
                state.interval = interval;
                state.completed
            };
            let _ = crate::doc_autosave::tick(
                world,
                crate::doc_autosave::TickInput {
                    project_root: &root,
                    now: std::time::Instant::now(),
                    force: false,
                },
            );
            let state = world.resource::<crate::doc_autosave::AutosaveState>();
            if state.completed != before
                && let Some(path) = &state.last_level
            {
                self.toast = format!("autosaved to {}", path.display());
            }
        }
        // Tools ▸ Bake reflection captures: one capture per frame, cancel between (ADR-7592).
        crate::capture_bake::tick(world);
    }

    /// Shares the crash-recovery offers with the world (`RecoveryOffers`, ADR-9770): the shell
    /// publishes its own changes, and adopts a change made in the world (an agent's
    /// `editor.recovery_restore` or `editor.recovery_discard`, or the recovery list window).
    ///
    /// When both changed since the last sync (an agent call and a window button in the same
    /// frame) and the offers are of the same project, both sides only removed rows, so the
    /// result keeps the rows still offered on both sides. A project opened since (the shell
    /// rescanned) replaces the world's offers.
    pub fn sync_offers(&mut self, world: &mut World) {
        crate::doc_autosave::install(world);
        let root = self.project_root();
        let world_revision = world.resource::<crate::doc_autosave::RecoveryOffers>().revision;
        let world_changed = self.offers_seen.is_some_and(|seen| seen != world_revision);
        let same_project = world
            .resource::<crate::doc_autosave::RecoveryOffers>()
            .project_root
            == root;
        if self.offers_changed && world_changed && same_project {
            let offers = world.resource::<crate::doc_autosave::RecoveryOffers>();
            let levels: Vec<std::path::PathBuf> = offers.levels.iter().map(|l| l.autosave.clone()).collect();
            let docs: Vec<String> = offers.docs.iter().map(|d| d.entry.id.clone()).collect();
            self.recovery.retain(|l| levels.contains(&l.autosave));
            self.recovery_docs.retain(|d| docs.contains(&d.entry.id));
        }
        let adopted = !self.offers_changed && world_changed;
        if adopted {
            let offers = world.resource::<crate::doc_autosave::RecoveryOffers>();
            self.recovery = offers.levels.clone();
            self.recovery_docs = offers.docs.clone();
            self.offers_seen = Some(world_revision);
        } else if self.offers_changed || self.offers_seen.is_none() {
            let mut offers = world.resource_mut::<crate::doc_autosave::RecoveryOffers>();
            offers.project_root = root;
            offers.levels = self.recovery.clone();
            offers.docs = self.recovery_docs.clone();
            offers.revision += 1;
            self.offers_seen = Some(offers.revision);
            self.offers_changed = false;
        }
    }
}

/// Whether a tab group floats in its own window (undocked).
fn dock_has_window(dock: &DockState<Panel>) -> bool {
    dock.iter_surfaces()
        .any(|surface| matches!(surface, egui_dock::Surface::Window(..)))
}

/// The undo side of a frame, after the panels drew: Ctrl+Z undoes, Ctrl+Shift+Z and Ctrl+Y redo
/// (Cmd on macOS; not while a text field has focus, which keeps its own undo, nor during play),
/// and a released pointer seals the stack so the next drag or scrub starts its own entry.
pub fn edit_shortcuts(ctx: &egui::Context, world: &mut World, playing: bool) {
    if !playing && !ctx.text_edit_focused() {
        let redo = ctx.input_mut(|i| {
            i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::Z,
            )) || i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::COMMAND,
                egui::Key::Y,
            ))
        });
        let undo = !redo
            && ctx.input_mut(|i| {
                i.consume_shortcut(&egui::KeyboardShortcut::new(
                    egui::Modifiers::COMMAND,
                    egui::Key::Z,
                ))
            });
        if redo {
            crate::command::redo_world(world);
        } else if undo {
            crate::command::undo_world(world);
        }
    }
    if ctx.input(|i| i.pointer.any_released()) {
        world
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .seal();
    }
}

/// The play shortcut (`specs/19` § 4): Alt+P while no session runs queues
/// [`crate::pie::PieRequest::Play`], like the toolbar's Play button. It does nothing while a session
/// runs (so a stray press cannot restart one) or while a text field has focus.
pub fn play_shortcuts(ctx: &egui::Context, world: &mut World, session_active: bool) {
    if session_active || ctx.text_edit_focused() {
        return;
    }
    let play = ctx
        .input_mut(|i| i.consume_shortcut(&egui::KeyboardShortcut::new(egui::Modifiers::ALT, egui::Key::P)));
    if play {
        world
            .get_resource_or_insert_with::<crate::pie::PieRequests>(Default::default)
            .push(crate::pie::PieRequest::Play);
    }
}

/// The label and request of the play toolbar's second button in `mode`: **⏸ Pause** while
/// stopped or playing (disabled while stopped), **▶ Resume** while paused. Before ADR-9810 the
/// button said Pause while paused and did nothing, so a paused session could not be resumed from
/// the toolbar (`docs/evidence/editor-a11y-2026-11.md`, E-1).
#[must_use]
pub fn pause_button(mode: Option<crate::pie::PlayMode>) -> (&'static str, crate::pie::PieRequest) {
    if mode == Some(crate::pie::PlayMode::Paused) {
        ("▶ Resume", crate::pie::PieRequest::Resume)
    } else {
        ("⏸ Pause", crate::pie::PieRequest::Pause)
    }
}

/// The play toolbar's buttons for a session in `mode` (`None`: stopped): **▶ Play**, **▶ Simulate**,
/// **⏸ Pause** or **▶ Resume** ([`pause_button`]), **⏹ Stop**, and **⏭ Step** while paused. Answers
/// the request of the button pressed this frame. The buttons are the registered editor actions
/// (`crate::agent_parity`), so `editor.actions` maps each to its agent method.
pub fn play_buttons(ui: &mut egui::Ui, mode: Option<crate::pie::PlayMode>) -> Option<crate::pie::PieRequest> {
    use crate::agent_parity::{
        PLAY_PAUSE, PLAY_PLAY, PLAY_RESUME, PLAY_STEP, PLAY_STOP, action_button, action_enabled,
    };
    let active = mode.is_some();
    let paused = mode == Some(crate::pie::PlayMode::Paused);
    let mut pressed = None;
    if action_button(ui, &PLAY_PLAY)
        .on_hover_text("Play the level in the editor (Alt+P)")
        .clicked()
    {
        pressed = Some(crate::pie::PieRequest::Play);
    }
    if crate::agent_parity::action_button(ui, &crate::agent_parity::PLAY_SIMULATE)
        .on_hover_text("Run physics, AI and scripts with no player; the camera stays free")
        .clicked()
    {
        pressed = Some(crate::pie::PieRequest::PlayWith(crate::pie::PlayOptions {
            kind: crate::pie::PlayKind::Simulate,
            ..crate::pie::PlayOptions::default()
        }));
    }
    let (_, request) = pause_button(mode);
    let pause = if paused { &PLAY_RESUME } else { &PLAY_PAUSE };
    if action_enabled(ui, active, pause, None).clicked() {
        pressed = Some(request);
    }
    if action_enabled(ui, active, &PLAY_STOP, None)
        .on_hover_text("Stop play (Escape)")
        .clicked()
    {
        pressed = Some(crate::pie::PieRequest::Stop);
    }
    if paused && action_button(ui, &PLAY_STEP).clicked() {
        pressed = Some(crate::pie::PieRequest::Step(1));
    }
    pressed
}

/// What a pressed Escape does in the editor window, after the panels had their turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeAction {
    /// A panel or text field holds the keyboard and uses Escape itself.
    Ignore,
    /// A play session (playing or paused) is running: Escape stops it (`specs/19` § 4).
    StopPlay,
    /// Nothing is running: Escape asks to close the editor, like the window's close button
    /// (ADR-2179; unsaved documents prompt first).
    Close,
    /// The full-screen viewport shows and no play session runs: Escape leaves full screen
    /// (`crate::workspaces::escape_action_full`, ADR-7753).
    LeaveFullscreen,
}

/// Decides what Escape does: a focused panel keeps it, a running play session stops, otherwise the
/// editor asks to close. Stopping play never closes the editor in the same press.
pub fn escape_action(session_active: bool, panel_wants_keyboard: bool) -> EscapeAction {
    if panel_wants_keyboard {
        EscapeAction::Ignore
    } else if session_active {
        EscapeAction::StopPlay
    } else {
        EscapeAction::Close
    }
}

/// Floated panels as OS windows (ADR-7821).
impl EditorShell {
    /// Performs the queued `OsWindowRequest`s (`editor.panel_float`, the tab menu, an OS window's
    /// close button, a lost monitor).
    fn apply_os_window_requests(&mut self, ctx: &egui::Context, world: &mut World) {
        use crate::os_windows::{OsWindowRequest, OsWindowRequests};
        let Some(requests) = world.remove_resource::<OsWindowRequests>() else {
            return;
        };
        for request in requests.pending {
            let line = match request {
                OsWindowRequest::Float(panel) => {
                    let at = ctx.pointer_latest_pos().unwrap_or(egui::pos2(96.0, 96.0));
                    let [w, h] = crate::os_windows::DEFAULT_WINDOW_SIZE;
                    // egui_dock shrinks a window torn from the main surface to 80 %.
                    let rect = egui::Rect::from_min_size(at, egui::vec2(w / 0.8, h / 0.8));
                    match crate::os_windows::float_panel(&mut self.dock, panel, rect) {
                        Ok(true) => format!("{} floats in its own window", panel.title()),
                        Ok(false) => format!("{} already floats", panel.title()),
                        Err(err) => err,
                    }
                }
                OsWindowRequest::Dock(panel) => match crate::os_windows::dock_panel(&mut self.dock, panel) {
                    Ok(true) => format!("{} docked back", panel.title()),
                    Ok(false) => format!("{} is docked", panel.title()),
                    Err(err) => err,
                },
                OsWindowRequest::FloatFocused => {
                    let Some(panel) = crate::workspaces::focused_tab(&self.dock) else {
                        continue;
                    };
                    let at = ctx.pointer_latest_pos().unwrap_or(egui::pos2(96.0, 96.0));
                    let [w, h] = crate::os_windows::DEFAULT_WINDOW_SIZE;
                    let rect = egui::Rect::from_min_size(at, egui::vec2(w / 0.8, h / 0.8));
                    match crate::os_windows::float_panel(&mut self.dock, panel, rect) {
                        Ok(true) => format!("{} floats in its own window", panel.title()),
                        Ok(false) => format!("{} already floats", panel.title()),
                        Err(err) => err,
                    }
                }
                OsWindowRequest::DockAll => {
                    let windows: Vec<egui_dock::SurfaceIndex> = self
                        .dock
                        .iter_surfaces_indexed()
                        .filter(|(_, s)| matches!(s, egui_dock::Surface::Window(..)))
                        .map(|(index, _)| index)
                        .collect();
                    let mut count = 0;
                    // Highest slot first: removing the last surface pops it.
                    for index in windows.into_iter().rev() {
                        count += crate::os_windows::dock_surface(&mut self.dock, index);
                    }
                    format!("{count} panel(s) docked back into the main window")
                }
                OsWindowRequest::DockWindow(serial) => match self.os.surface_of(serial) {
                    Some(index) => {
                        let count = crate::os_windows::dock_surface(&mut self.dock, index);
                        format!("{count} panel(s) docked back into the main window")
                    }
                    None => continue,
                },
            };
            self.toast = line;
        }
    }

    /// The serial of OS window `id` (`None`: not an OS window of this shell, or closing).
    #[must_use]
    pub fn os_window_serial(&self, id: egui::ViewportId) -> Option<u64> {
        self.os.serial_of_viewport(id)
    }

    /// Whether OS window `serial` came from a saved layout and has not been placed yet (the runner
    /// re-docks it when its monitor is gone).
    #[must_use]
    pub fn os_window_restored(&self, serial: u64) -> bool {
        self.os.restored(serial)
    }

    /// The runner opened OS window `serial` where it belongs.
    pub fn os_window_placed(&mut self, serial: u64) {
        self.os.placed(serial);
    }

    /// Draws OS window `id`'s panels: the editor binary runs one egui pass per child viewport the
    /// root pass's output lists and calls this inside it (ADR-7821). The window's surface is drawn
    /// as the main surface of a temporary dock and put back; tabs torn off inside it become new
    /// windows. Nothing draws for an id this shell does not know (a window that is closing).
    #[tracing::instrument(target = "regia_editor", skip_all, name = "editor_os_window_ui")]
    pub fn os_window_ui(
        &mut self,
        ui: &mut egui::Ui,
        id: egui::ViewportId,
        world: &mut World,
        play: Option<&mut World>,
    ) {
        use egui_dock::{Surface, SurfaceIndex};
        let Some(serial) = self.os.serial_of_viewport(id) else {
            return;
        };
        let Some(index) = self.os.surface_of(serial) else {
            return;
        };
        let ctx = ui.ctx().clone();
        self.os.observe(serial, &ctx);
        let Some(slot) = self.dock.get_surface_mut(index) else {
            return;
        };
        let (tree, state) = match std::mem::replace(slot, Surface::Empty) {
            Surface::Window(tree, state) => (tree, state),
            other => {
                *slot = other;
                return;
            }
        };
        let mut temp = DockState::new(Vec::new());
        *temp.main_surface_mut() = tree;
        {
            let EditorShell {
                log,
                content,
                material,
                settings,
                viewport,
                asset_viewer,
                device,
                instances,
                ..
            } = self;
            let mut viewer = PanelViewer {
                world: &mut *world,
                play,
                log,
                content,
                material,
                settings,
                viewport,
                asset_viewer,
                device: device.as_ref(),
                in_os_window: true,
                instances,
            };
            let mut dock_style = crate::theme::dock_style(ui.style().as_ref());
            crate::workspaces::reserve_tab_buttons(&mut dock_style);
            let bar_height = dock_style.tab_bar.height;
            egui::CentralPanel::default().show(ui, |ui| {
                DockArea::new(&mut temp)
                    .id(egui::Id::new(("regia.os_window_dock", serial)))
                    .style(dock_style)
                    .show_inside(ui, &mut viewer);
                crate::workspaces::window_tab_buttons(ui, viewer.world, &temp, bar_height, index);
            });
        }
        let tree = std::mem::take(temp.main_surface_mut());
        if tree.num_tabs() == 0 {
            self.dock.remove_surface(index);
        } else if let Some(slot) = self.dock.get_surface_mut(index) {
            *slot = Surface::Window(tree, state);
        }
        // Tabs torn off inside this window: new windows of the main dock (new OS windows next frame).
        for at in (1..temp.surfaces_count()).rev() {
            if let Some(surface @ Surface::Window(..)) = temp.remove_surface(SurfaceIndex(at)) {
                let new = self.dock.add_window(Vec::new());
                if let Some(slot) = self.dock.get_surface_mut(new) {
                    *slot = surface;
                }
            }
        }
        crate::workspaces::DockGroups::refresh(world, &self.dock);
        self.publish_layout(world);
    }
}

/// The dock's tab viewer: routes each panel to its draw call.
struct PanelViewer<'a> {
    world: &'a mut World,
    /// The play-in-editor world, while a session runs (`None` while stopped).
    play: Option<&'a mut World>,
    log: &'a mut LogPanelState,
    content: &'a mut ContentBrowserState,
    material: &'a mut MaterialEditorState,
    settings: &'a mut ProjectSettingsState,
    viewport: &'a mut crate::viewport::ViewportPanelState,
    asset_viewer: &'a mut crate::asset_viewer::AssetViewerState,
    device: Option<&'a RenderDevice>,
    /// The dock draws inside an OS window of its own (ADR-7821).
    in_os_window: bool,
    /// The Content Browser instances past the first (P-003).
    instances: &'a mut crate::panel_instances::ShellInstances,
}

impl PanelViewer<'_> {
    /// Draws instance `n` (`>= 1`) of `kind` with its own state (P-003, ADR-8105 (a)): the same
    /// panel code as instance 0, scoped by `crate::panel_instances::with_instance`.
    fn instance_ui(&mut self, ui: &mut egui::Ui, kind: InstancedPanel, n: u8) {
        let panel = Panel::Instance(kind, n);
        match kind {
            InstancedPanel::Outliner => {
                match crate::remote_world::source_switch(ui, self.world, crate::remote_world::Pane::Outliner)
                {
                    crate::remote_world::Shown::Remote => {}
                    crate::remote_world::Shown::Edit => {
                        crate::panel_instances::with_instance(self.world, panel, |world| {
                            outliner_panel(ui, world);
                        });
                    }
                    crate::remote_world::Shown::Play => {
                        crate::pie_worlds::with_viewed_world(self.world, self.play.as_deref_mut(), |world| {
                            crate::panel_instances::with_instance(world, panel, |world| {
                                outliner_panel(ui, world);
                            });
                        });
                    }
                }
            }
            InstancedPanel::Inspector => {
                let content = &*self.content;
                match crate::remote_world::source_switch(ui, self.world, crate::remote_world::Pane::Inspector)
                {
                    crate::remote_world::Shown::Remote => {}
                    crate::remote_world::Shown::Edit => {
                        crate::panel_instances::with_instance(self.world, panel, |world| {
                            crate::inspector::refresh_asset_catalog(world, content);
                            crate::inspector::inspector_panel(ui, world);
                        });
                    }
                    crate::remote_world::Shown::Play => {
                        crate::pie_worlds::with_viewed_world(self.world, self.play.as_deref_mut(), |world| {
                            crate::panel_instances::with_instance(world, panel, |world| {
                                crate::inspector::refresh_asset_catalog(world, content);
                                crate::inspector::inspector_panel(ui, world);
                            });
                        });
                    }
                }
            }
            InstancedPanel::Content => {
                let root = self.content.project_root.clone();
                let browser = self
                    .instances
                    .content
                    .entry(n)
                    .or_insert_with(|| ContentBrowserState {
                        project_root: root.clone(),
                        ..Default::default()
                    });
                // Another project may have opened since the instance was made.
                browser.project_root = root;
                let mut open = self
                    .world
                    .get_resource::<OpenLevel>()
                    .cloned()
                    .unwrap_or_else(|| level_io::new_level(self.world));
                crate::panel_instances::with_instance(self.world, panel, |world| {
                    crate::content::content_browser_panel(ui, world, browser, &mut open);
                });
                self.world.insert_resource(open);
                // What the shell acts on after the dock (open, view, play a video, an audit
                // action) goes through the first browser's requests, as if it had been asked there.
                let first = &mut *self.content;
                if let Some(path) = browser.open_request.take() {
                    first.open_request = Some(path);
                }
                if let Some(path) = browser.view_request.take() {
                    first.view_request = Some(path);
                }
                if let Some(path) = browser.video_request.take() {
                    first.video_request = Some(path);
                }
                if let Some(action) = browser.asset_action.take() {
                    first.asset_action = Some(action);
                }
            }
        }
    }
}

/// The viewport while the separate play window shows play (ADR-7283): no render, a note.
fn play_window_placeholder(ui: &mut egui::Ui, status: &crate::play_window::PlayWindowStatus) {
    let [w, h] = status.render_size;
    ui.centered_and_justified(|ui| {
        ui.label(format!(
            "▶ Playing in the separate window ({w} × {h}, {}).\n\
             The viewport does not draw while it is open, so play renders once per frame.\n\
             Close the window or press ⏹ Stop to end play.",
            status.resolution.preset_id().replace('_', " ")
        ));
    });
}

/// The toolbar's play resolution menu (ADR-7283): presets and a custom size, stored in the
/// editor preferences (`play.resolution`); the next play window renders at it.
fn play_resolution_menu(ui: &mut egui::Ui, world: &mut World) {
    use crate::play_window::{PlayResolution, RESOLUTION_PRESETS};
    let prefs = world
        .get_resource::<crate::editor_prefs::EditorPreferences>()
        .cloned()
        .unwrap_or_default();
    let current = PlayResolution::from_prefs(&prefs);
    let label = match current.size() {
        Some([w, h]) => format!("⛶ {w} × {h}"),
        None => "⛶ Window size".to_owned(),
    };
    ui.menu_button(label, |ui| {
        for preset in RESOLUTION_PRESETS {
            if ui
                .selectable_label(current == preset.resolution, preset.label)
                .clicked()
            {
                crate::play_window::set_play_resolution(world, preset.resolution);
                ui.close();
            }
        }
        ui.separator();
        let side = |key: &str| u32::try_from(prefs.int(key)).unwrap_or(0);
        let mut width = side(crate::play_window::PREF_RESOLUTION_WIDTH);
        let mut height = side(crate::play_window::PREF_RESOLUTION_HEIGHT);
        let range = crate::play_window::MIN_RESOLUTION..=crate::play_window::MAX_RESOLUTION;
        ui.horizontal(|ui| {
            ui.label("Custom");
            let w = ui.add(egui::DragValue::new(&mut width).range(range.clone()));
            ui.label("×");
            let h = ui.add(egui::DragValue::new(&mut height).range(range));
            let custom = current.preset_id() == crate::play_window::CUSTOM_RESOLUTION;
            let edited = w.changed() || h.changed();
            let used = ui.button("Use").clicked();
            let Some(resolution) = PlayResolution::custom(width, height) else {
                return;
            };
            if used || (edited && custom) {
                crate::play_window::set_play_resolution(world, resolution);
            } else if edited {
                // Kept for when the custom size is chosen.
                let mut prefs =
                    world.get_resource_or_insert_with(crate::editor_prefs::EditorPreferences::default);
                for (key, value) in [
                    (crate::play_window::PREF_RESOLUTION_WIDTH, width),
                    (crate::play_window::PREF_RESOLUTION_HEIGHT, height),
                ] {
                    let _ = prefs.set(key, &crate::editor_prefs::PrefValue::Int(i64::from(value)));
                }
            }
        });
    })
    .response
    .on_hover_text("The size the play window renders at (scaled to fit, clicks map back)");
}

impl TabViewer for PanelViewer<'_> {
    type Tab = Panel;

    fn id(&mut self, tab: &mut Self::Tab) -> egui::Id {
        egui::Id::new(*tab)
    }

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        match tab {
            Panel::GameView(role) => crate::pie_process_view::tab_title(*role).into(),
            _ => tab.title().into(),
        }
    }

    fn on_close(&mut self, tab: &mut Self::Tab) -> egui_dock::tab_viewer::OnCloseResponse {
        // Closing a Game tab stops its stream; the process keeps running (ADR-7831).
        if let Panel::GameView(role) = tab {
            crate::pie_process_view::close(self.world, *role, false);
        }
        egui_dock::tab_viewer::OnCloseResponse::Close
    }

    // The add-tab popup, the tab menu and drag dwell (ADR-7753).
    fn add_popup(&mut self, ui: &mut egui::Ui, path: egui_dock::NodePath) {
        let tabs = self
            .world
            .get_resource::<crate::workspaces::DockGroups>()
            .map(|groups| groups.tabs(path).to_vec())
            .unwrap_or_default();
        crate::workspaces::add_popup(ui, self.world, &tabs);
    }

    fn context_menu(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab, path: egui_dock::NodePath) {
        crate::workspaces::tab_context_menu(ui, self.world, *tab);
        // Float in own window / Dock back (ADR-7821).
        crate::os_windows::tab_menu(ui, self.world, *tab, self.in_os_window || !path.surface.is_main());
    }

    fn on_tab_button(&mut self, tab: &mut Self::Tab, response: &egui::Response) {
        crate::workspaces::tab_drag_dwell(self.world, *tab, response);
        crate::workspaces::note_tab_rect(self.world, *tab, response);
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        // Each dock panel is a named pane for screen readers (ADR-7746).
        crate::a11y_bridge::region(ui, egui::accesskit::Role::Pane, tab.title());
        // Every panel's draw time, for `editor.panel_timings` (ADR-7306).
        let _timer = crate::panel_timings::PanelTimer::start(tab.title());
        if let Some(mut observation) = self
            .world
            .get_resource_mut::<crate::editor_layout::LayoutObservation>()
        {
            observation.record_panel(tab.id(), ui);
        }
        match tab {
            Panel::Viewport => {
                // The play window shows the play world (ADR-7283): the viewport draws a placeholder
                // instead, so play renders once per frame.
                let window = self
                    .world
                    .get_resource::<crate::play_window::PlayWindowStatus>()
                    .filter(|status| status.open)
                    .cloned();
                if let (Some(status), Some(_)) = (window, self.play.as_ref()) {
                    play_window_placeholder(ui, &status);
                    return;
                }
                // Field-disjoint borrows: the play world when playing, the edit world otherwise.
                let rect = ui.max_rect();
                let world = self.play.as_deref_mut().unwrap_or(&mut *self.world);
                viewport_panel(ui, world, self.device, self.viewport);
                // The play border (ADR-7829): edits made in play are obvious.
                if self.play.is_some()
                    && let Some(color) = crate::pie_controls::play_tint(self.world)
                {
                    crate::pie_controls::paint_viewport_border(ui.painter(), rect, color);
                }
            }
            // With client worlds, the Outliner and Inspector show the viewed role's world (ADR-7829).
            Panel::Outliner => {
                // The source switch (ADR-7833): a runtime's tree is drawn by the switch itself; the
                // play world is the viewed client's (3247).
                match crate::remote_world::source_switch(ui, self.world, crate::remote_world::Pane::Outliner)
                {
                    crate::remote_world::Shown::Remote => {}
                    crate::remote_world::Shown::Edit => outliner_panel(ui, &mut *self.world),
                    crate::remote_world::Shown::Play => {
                        crate::pie_worlds::with_viewed_world(self.world, self.play.as_deref_mut(), |world| {
                            outliner_panel(ui, world);
                        });
                    }
                }
            }
            Panel::Inspector => {
                let content = &*self.content;
                match crate::remote_world::source_switch(ui, self.world, crate::remote_world::Pane::Inspector)
                {
                    crate::remote_world::Shown::Remote => {}
                    crate::remote_world::Shown::Edit => {
                        crate::inspector::refresh_asset_catalog(self.world, content);
                        crate::inspector::inspector_panel(ui, self.world);
                    }
                    crate::remote_world::Shown::Play => {
                        crate::pie_worlds::with_viewed_world(self.world, self.play.as_deref_mut(), |world| {
                            crate::inspector::refresh_asset_catalog(world, content);
                            crate::inspector::inspector_panel(ui, world);
                        });
                    }
                }
            }
            // The content drawer draws this browser after the dock: the tab gives way (ADR-8702).
            Panel::Content if crate::drawers::content_in_drawer(self.world) => {
                crate::drawers::docked_content_note(ui);
            }
            Panel::Content => {
                let mut open = self
                    .world
                    .get_resource::<OpenLevel>()
                    .cloned()
                    .unwrap_or_else(|| level_io::new_level(self.world));
                crate::content::content_browser_panel(ui, self.world, self.content, &mut open);
                self.world.insert_resource(open);
            }
            Panel::Material => material_editor_panel(ui, self.material, self.device),
            Panel::ProjectSettings => {
                let root = self.content.project_root.clone();
                // The Localization, Packaging and Audio pages (ADR-7234); physics layers are the
                // panel's own matrix section (ADR-7267).
                if let Some(root) = root.as_deref() {
                    crate::creators::project_settings_pages(ui, self.world, self.settings, root);
                    // Validation: the pre-commit hook (ADR-7789).
                    crate::project_health::validation_settings_ui(ui, self.world, root);
                    // The package icon picker (assets/branding/icon.png, ADR-7475).
                    crate::project_icon::icon_picker_ui(ui, self.world, root);
                }
                let mut simulator = self
                    .world
                    .get_resource_or_init::<crate::xr_simulator::XrSimulator>();
                self.settings.xr.simulator = simulator.toggle;
                project_settings_panel(ui, self.settings, root.as_deref());
                simulator.toggle = self.settings.xr.simulator;
                // Typed project settings of extensions (ADR-7791).
                crate::extension::settings::project_settings_section(ui, self.world, self.settings);
            }
            Panel::OutputLog => {
                crate::launch::launch_bar(ui, self.world);
                // The console line under the log (ADR-7754).
                if let Some(link) = crate::log_panel::output_log_with_console(ui, self.log, self.world, "tab")
                {
                    self.log.status = crate::log_links::open_clicked(self.world, &link);
                }
            }
            Panel::AssetViewer => {
                let root = self.content.project_root.clone();
                let library = self
                    .world
                    .get_resource_or_insert_with(crate::mesh_source::MeshLibrary::default)
                    .clone();
                crate::asset_viewer::asset_viewer_panel(
                    ui,
                    self.asset_viewer,
                    root.as_deref(),
                    &library,
                    self.device,
                );
                // The mesh helper buttons and Content Browser requests that need the world (ADR-7771).
                crate::asset_viewer::run_helper_requests(ui.ctx(), self.asset_viewer, self.world);
            }
            Panel::AnimGraph => {
                // PIE debugging: mirror the play world's animator running this graph.
                crate::anim::sync_pie_debug(self.world, self.play.as_deref_mut());
                crate::anim::graph_panel::anim_graph_panel(ui, self.world, self.device);
            }
            Panel::AnimClip => crate::anim::clip_panel::anim_clip_panel(ui, self.world, self.device),
            Panel::AnimSkeleton => {
                crate::anim::skeleton_panel::anim_skeleton_panel(ui, self.world, self.device)
            }
            Panel::Kit => crate::kit_view::kit_panel(ui, self.world, self.device),
            Panel::Bake => crate::bake_panel::bake_panel(ui, self.world),
            Panel::BakeRunner => crate::bake_runner::bake_runner_panel(ui, self.world),
            Panel::Lightmaps => crate::lightmap_panel::lightmap_panel(ui, self.world),
            Panel::Theme => crate::theme_panel::theme_panel(ui, self.world),
            Panel::Shots => crate::shot_panel::shot_panel(ui, self.world),
            Panel::Net => crate::pie_net::net_panel_with(ui, self.world, self.device),
            Panel::Tool => crate::tools::panel::tools_panel(ui, self.world),
            Panel::Mode2d => crate::mode2d::panel::mode2d_panel(ui, self.world),
            Panel::DccFeed => crate::dcc_feed::dcc_feed_panel(ui, self.world),
            Panel::ProcGraph => crate::procgen::panel::procgen_graph_panel(ui, self.world),
            Panel::ContentChunks => crate::chunks_panel::content_chunks_panel(ui, self.world),
            Panel::Extensions => crate::extension::panel::extensions_panel(ui, self.world),
            Panel::ExtensionPanel(id, _) => crate::extension::panel::extension_tab(ui, self.world, id),
            Panel::GameView(role) => crate::pie_process_view::game_tab(ui, self.world, *role),
            Panel::Instance(kind, n) => {
                let (kind, n) = (*kind, *n);
                ui.push_id(("panel-instance", kind, n), |ui| self.instance_ui(ui, kind, n));
            }
            Panel::SeqCurves => {
                crate::seq_curve_panel::seq_curve_panel_with_play(ui, self.world, self.play.as_deref_mut())
            }
            Panel::UiDesigner => crate::ui_designer_panel::ui_designer_panel(ui, self.world),
            Panel::UiAnim => crate::ui_anim_panel::ui_anim_panel(ui, self.world),
            Panel::Fracture => crate::fracture_panel::fracture_panel(ui, self.world),
            Panel::UndoHistory => crate::history_panel::history_panel(ui, self.world),
            Panel::Tables => crate::tables_panel::tables_panel(ui, self.world),
            Panel::Abilities => crate::abilities_panel::abilities_panel(ui, self.world),
            Panel::Montage => crate::montage_panel::montage_panel(ui, self.world),
            Panel::Ragdoll => crate::ragdoll_panel::ragdoll_panel(ui, self.world),
            Panel::Problems => crate::asset_panels::problems_panel(ui, self.world),
            Panel::ReferenceGraph => crate::asset_panels::reference_graph_panel(ui, self.world),
            Panel::SizeMap => crate::asset_panels::size_map_panel(ui, self.world),
            Panel::Layers => crate::layers_panel::layers_panel(ui, self.world),
            Panel::Tests => crate::tests_panel::tests_panel(ui, self.world),
            Panel::StateTrees => crate::state_tree_panel::state_tree_panel(ui, self.world),
            Panel::SmartObjects => crate::smart_object_panel::smart_object_panel(ui, self.world),
            Panel::Cameras => crate::camera_panel::camera_panel(ui, self.world),
            Panel::MaterialGraph => {
                crate::matgraph_editor::panel::matgraph_editor_panel(ui, self.world, self.device);
            }
            Panel::AudioMixer => crate::audio_mixer_panel::audio_mixer_panel(ui, self.world),
            Panel::CueEditor => crate::cue_editor_panel::cue_editor_panel(ui, self.world),
            Panel::AgentChanges => crate::agent_changes::agent_changes_panel(ui, self.world),
            Panel::Agent => crate::agent_panel::panel(ui, self.world),
            Panel::Preferences => crate::prefs_panel::preferences_panel(ui, self.world),
            Panel::Terrain => crate::terrain_panel::terrain_panel(ui, self.world),
            Panel::BehaviorTree => crate::bt_editor::panel::bt_editor_panel(ui, self.world),
            Panel::MotionMatching => crate::mm_panel::mm_panel(ui, self.world),
            Panel::UiInspector => {
                crate::ui_inspector::ui_inspector_panel(ui, self.world, self.play.as_deref_mut())
            }
            Panel::WorldSettings => crate::world_settings::world_settings_panel(ui, self.world),
            Panel::VfxEditor => crate::vfx_editor::panel::vfx_editor_panel(ui, self.world, self.device),
            Panel::StateTreeEditor => {
                crate::state_tree_editor::panel::state_tree_editor_panel(ui, self.world)
            }
            Panel::Video => crate::video_panel::video_panel(ui, self.world),
            Panel::Conversation => crate::conversation_panel::panel::conversation_panel(ui, self.world),
            Panel::Localization => crate::loc_panel::loc_panel(ui, self.world, self.play.as_deref_mut()),
            Panel::Cloth => crate::cloth_panel::cloth_panel(ui, self.world),
            Panel::LookDev => crate::lookdev_panel::lookdev_panel(ui, self.world),
            Panel::DataAsset => crate::data_asset::data_asset_panel(ui, self.world),
            Panel::VehicleTuning => crate::vehicle_tuning_panel::vehicle_tuning_panel(ui, self.world),
            Panel::LogicGraph => crate::logic_graph_editor::panel::logic_graph_panel(ui, self.world),
            Panel::SmartObjectDef => crate::smart_object_def_panel::smart_object_def_panel(ui, self.world),
            Panel::Biome => crate::biome_panel::biome_panel(ui, self.world),
            Panel::BulkEdit => crate::bulk_edit_panel::bulk_edit_panel(ui, self.world),
            Panel::ProjectHealth => crate::project_health::project_health_panel(ui, self.world),
            Panel::Retarget => crate::anim::retarget_panel::retarget_panel(ui, self.world),
            Panel::WorldMap => crate::world_panel::world_map_panel(ui, self.world),
            Panel::SelectedDebug => crate::debug_overlay_selected::show(ui, self.world),
            Panel::VisualLog => crate::visual_log_panel::show(ui, self.world),
            Panel::Notes => crate::review_notes::panel(ui, self.world),
            Panel::Activity => crate::activity_panel::panel(ui, self.world),
            Panel::MatchRules => crate::match_rules_panel::match_rules_panel(ui, self.world),
            Panel::UtilityPanel(id, _) => crate::utility_panel::utility_panel_tab(ui, self.world, id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ecs::ecs::entity::Entity;

    /// Review sprint-v06-rev-late-k #10: while a tab is maximized, an extension panel that
    /// becomes active lands in the dock the maximize replaced (the shown dock keeps one tab); a
    /// Tools focus request restores first and the restored dock keeps the extension tab; a
    /// maximized extension panel whose extension goes away is restored, not left dead.
    #[test]
    fn background_tab_changes_while_maximized_keep_the_one_tab_dock() {
        use crate::workspaces::ViewRequest;
        let mut shell = EditorShell::new(None, None);
        let mut world = World::new();
        let ext = Panel::ExtensionPanel("test.ext", "Ext");
        shell
            .views
            .apply(
                &mut shell.dock,
                &mut world,
                ViewRequest::ToggleMaximize(Some(Panel::Viewport)),
            )
            .expect("maximize");
        assert_eq!(shell.dock.iter_all_tabs().count(), 1);
        shell.apply_extension_tabs(vec![("test.ext", "Ext")], Vec::new());
        assert_eq!(
            shell.dock.iter_all_tabs().count(),
            1,
            "the maximized view keeps its one tab"
        );
        assert_eq!(shell.views.maximized(), Some(Panel::Viewport));
        let mut host = crate::tools::ToolHost::default();
        host.focus_panel = true;
        world.insert_resource(host);
        shell.tool_focus(&mut world);
        assert_eq!(shell.views.maximized(), None, "a focus request restores first");
        assert!(shell.dock.find_tab(&Panel::Tool).is_some());
        assert!(
            shell.dock.find_tab(&ext).is_some(),
            "the restored dock kept the extension tab"
        );
        // A maximized extension panel whose extension went away.
        shell
            .views
            .apply(
                &mut shell.dock,
                &mut world,
                ViewRequest::ToggleMaximize(Some(ext)),
            )
            .expect("maximize the extension panel");
        shell.apply_extension_tabs(Vec::new(), vec!["test.ext"]);
        assert_eq!(shell.views.maximized(), None);
        assert!(shell.dock.find_tab(&ext).is_none(), "no dead tab");
        assert!(shell.dock.find_tab(&Panel::Viewport).is_some());
    }

    /// E-1 (docs/evidence/editor-a11y-2026-11.md): while paused the toolbar offers Resume, and every
    /// play button is named and reachable from the keyboard (Tab to it, Enter presses it).
    #[test]
    fn a_paused_session_resumes_from_the_keyboard() {
        use crate::pie::{PieRequest, PlayMode};
        assert_eq!(
            pause_button(Some(PlayMode::Paused)),
            ("▶ Resume", PieRequest::Resume)
        );
        assert_eq!(
            pause_button(Some(PlayMode::Playing)),
            ("⏸ Pause", PieRequest::Pause)
        );
        assert_eq!(pause_button(None).1, PieRequest::Pause);

        let key = |key: egui::Key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        // The names a screen reader reads.
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.horizontal(|ui| {
                let _ = play_buttons(ui, Some(PlayMode::Paused));
            });
        });
        output.textures_delta.clear();
        let update = output.platform_output.accesskit_update.expect("accesskit is on");
        let names: Vec<String> = crate::a11y_audit::widget_names(&update)
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        for name in ["▶ Play", "▶ Resume", "⏹ Stop", "⏭ Step"] {
            assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
        }
        assert!(
            !names.iter().any(|n| n == "⏸ Pause"),
            "no Pause while paused: {names:?}"
        );
        assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());

        // Tab k times, then Enter: which request does each focus stop press?
        let mut reached = Vec::new();
        for tabs in 1..=6 {
            let ctx = egui::Context::default();
            let mut pressed = None;
            let mut events: Vec<Vec<egui::Event>> = vec![Vec::new()];
            events.extend((0..tabs).map(|_| vec![key(egui::Key::Tab)]));
            events.push(vec![key(egui::Key::Enter)]);
            events.push(Vec::new());
            for frame in events {
                let raw = egui::RawInput {
                    events: frame,
                    ..Default::default()
                };
                let mut output = ctx.run_ui(raw, |ui| {
                    ui.horizontal(|ui| {
                        if let Some(request) = play_buttons(ui, Some(PlayMode::Paused)) {
                            pressed = Some(request);
                        }
                    });
                });
                output.textures_delta.clear();
            }
            reached.extend(pressed);
        }
        for request in [PieRequest::Resume, PieRequest::Stop, PieRequest::Step(1)] {
            assert!(
                reached.contains(&request),
                "{request:?} by Tab + Enter: {reached:?}"
            );
        }
    }

    #[test]
    fn default_layout_contains_every_panel() {
        let shell = EditorShell::new(None, None);
        let found: std::collections::HashSet<Panel> =
            shell.dock.iter_all_tabs().map(|(_, tab)| *tab).collect();
        for panel in [
            Panel::Viewport,
            Panel::Outliner,
            Panel::Inspector,
            Panel::Content,
            Panel::OutputLog,
            Panel::Material,
            Panel::ProjectSettings,
            Panel::AssetViewer,
            Panel::AnimGraph,
            Panel::AnimClip,
            Panel::AnimSkeleton,
            Panel::Kit,
            Panel::Bake,
            Panel::BakeRunner,
            Panel::Lightmaps,
            Panel::Theme,
            Panel::Shots,
            Panel::Net,
            Panel::Tool,
            Panel::Mode2d,
            Panel::DccFeed,
            Panel::ProcGraph,
            Panel::ContentChunks,
            Panel::Extensions,
            Panel::SeqCurves,
            Panel::UiDesigner,
            Panel::UiAnim,
            Panel::Fracture,
            Panel::Tables,
            Panel::Abilities,
            Panel::Montage,
            Panel::Ragdoll,
            Panel::Problems,
            Panel::ReferenceGraph,
            Panel::SizeMap,
            Panel::Layers,
            Panel::Tests,
            Panel::StateTrees,
            Panel::SmartObjects,
            Panel::Cameras,
            Panel::MaterialGraph,
            Panel::AudioMixer,
            Panel::CueEditor,
            Panel::AgentChanges,
            Panel::Terrain,
            Panel::BehaviorTree,
            Panel::MotionMatching,
            Panel::UiInspector,
            Panel::VfxEditor,
            Panel::StateTreeEditor,
            Panel::Video,
            Panel::Conversation,
            Panel::Localization,
            Panel::LookDev,
            Panel::VehicleTuning,
            Panel::Biome,
            Panel::ProjectHealth,
            Panel::Retarget,
            Panel::WorldMap,
        ] {
            assert!(found.contains(&panel), "{panel:?} missing from the layout");
        }
    }

    /// w7 item 4: with auto key on and the Sequence Curves panel closed, a gizmo move is keyed by
    /// the next shell frame, exactly once; with the panel drawn in the same frame, still once.
    #[test]
    fn auto_key_runs_once_per_shell_frame_with_the_sequencer_panel_closed_or_open() {
        use regia_app::prelude::App;
        use regia_core::id::Guid;
        use regia_framework::sequencer::SequencePlayer;
        use regia_scene::prelude::{EntityGuid, Transform};

        let call = |world: &mut World, method: &str, params: serde_json::Value| {
            let (_, handler) = crate::agent_ext::all_methods()
                .into_iter()
                .find(|(schema, _)| schema.name == method)
                .unwrap_or_else(|| panic!("{method} is installed"));
            handler(world, &params).unwrap_or_else(|e| panic!("{method}: {}", e.message))
        };
        let keys_of = |world: &World, player: Entity| -> usize {
            world
                .get::<SequencePlayer>(player)
                .map_or(0, |p| p.sequence().tracks.iter().map(|t| t.keys.len()).sum())
        };
        let gizmo_move = |world: &mut World, entity: Entity, x: f32| {
            let mut new = world.get::<Transform>(entity).copied().unwrap_or_default();
            new.translation.x = x;
            let mut command = crate::command::SetTransform::capture(world, entity, new);
            command.apply(world);
            let mut undo = world.get_resource_or_insert_with(UndoStack::new);
            undo.push(Box::new(command));
            undo.seal();
        };

        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let world = app.world_mut();
        let open = level_io::new_level(world);
        world.insert_resource(open);
        let sequence =
            regia_framework::sequencer::edit::tracks::new_sequence("intro", 5.0, 30.0).expect("new");
        let player = world.spawn(SequencePlayer::new(sequence)).id();
        let cube = world
            .spawn((Transform::default(), EntityGuid(Guid::new_random())))
            .id();
        call(
            world,
            "editor.seq_binding_edit",
            serde_json::json!({ "op": "add", "binding": "cube", "entity": cube.to_bits() }),
        );
        call(world, "editor.seq_key", serde_json::json!({ "auto": true }));
        assert_eq!(keys_of(world, player), 0);

        // Panel closed: the shell frame keys the move, once.
        let mut shell = EditorShell::new(None, None);
        let ctx = egui::Context::default();
        let edits = world.resource::<UndoStack>().undo_depth();
        gizmo_move(world, cube, 1.0);
        shell.frame_effects(&ctx, world);
        assert_eq!(keys_of(world, player), 3, "position, rotation and scale keyed");
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            edits + 1,
            "the keys joined the move's undo entry"
        );
        shell.frame_effects(&ctx, world);
        assert_eq!(keys_of(world, player), 3, "a second frame keys nothing new");

        // Panel open: it draws (egui may run the UI twice) and the shell frame runs; one key set.
        call(
            world,
            "editor.seq_transport",
            serde_json::json!({ "op": "seek", "time": 1.0 }),
        );
        gizmo_move(world, cube, 2.0);
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default().show(ui, |ui| crate::seq_curve_panel::seq_curve_panel(ui, world));
        });
        output.textures_delta.clear();
        shell.frame_effects(&ctx, world);
        shell.frame_effects(&ctx, world);
        assert_eq!(keys_of(world, player), 6, "the second move keyed once at 1 s");
    }

    #[test]
    fn dirty_level_autosaves_on_the_tick() {
        use regia_app::prelude::App;
        use regia_core::id::Guid;
        use regia_scene::prelude::Transform;

        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("regia-editor-shell-autosave-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets")).expect("dir");

        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let open = level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        let mut command =
            SpawnEntities::new_entity(app.world(), Guid::new_random(), "Rock", Transform::default())
                .expect("spawn");
        command.apply(app.world_mut());
        app.world_mut()
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .push(Box::new(command));

        let mut shell = EditorShell::new(None, Some(dir.clone()));
        shell.set_autosave_every(std::time::Duration::ZERO);
        let ctx = egui::Context::default();
        shell.frame_effects(&ctx, app.world_mut());
        // The write runs on the autosave worker (ADR-9770): wait for it.
        crate::doc_autosave::flush(app.world_mut(), std::time::Duration::from_secs(60));

        let autosave = dir.join(".regia/autosave/untitled.level.ron");
        assert!(autosave.is_file(), "the tick wrote an autosave");
        let text = std::fs::read_to_string(&autosave).expect("read");
        assert!(text.contains("Rock"), "the autosave has the edit: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A level app with one undoable spawn named `name` on the stack (the level is dirty).
    fn dirty_level_app(name: &str) -> regia_app::prelude::App {
        use regia_core::id::Guid;
        use regia_scene::prelude::Transform;
        let mut app = regia_app::prelude::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let open = level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        let mut command =
            SpawnEntities::new_entity(app.world(), Guid::new_random(), name, Transform::default())
                .expect("spawn");
        command.apply(app.world_mut());
        app.world_mut()
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .push(Box::new(command));
        app
    }

    /// Monkey seeds 0x5eed0002 and 0xc0ffee03 (ADR-3290): two floating windows whose tab bars
    /// overflow drew egui_dock's scroll handle with one id on two layers, which panics in debug
    /// builds. With a window open the shell turns the bar off.
    #[test]
    fn floating_windows_with_overflowing_tab_bars_draw() {
        let dir = std::env::temp_dir().join(format!("regia-editor-shell-windows-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        std::fs::write(
            dir.join("Regia.toml"),
            "[project]\nname = \"W\"\nid = \"com.example.w\"\n",
        )
        .expect("manifest");
        let mut app = dirty_level_app("Rock");
        let mut shell = EditorShell::new(None, Some(dir.clone()));
        assert!(!dock_has_window(&shell.dock), "the default layout has no window");
        let groups = [
            [
                Panel::OutputLog,
                Panel::Layers,
                Panel::Problems,
                Panel::SizeMap,
                Panel::Tests,
            ],
            [
                Panel::Theme,
                Panel::Extensions,
                Panel::AgentChanges,
                Panel::ContentChunks,
                Panel::ReferenceGraph,
            ],
        ];
        for group in groups {
            for panel in group {
                if let Some(path) = shell.dock.find_tab(&panel) {
                    let _ = shell.dock.remove_tab(path);
                }
            }
            let window = shell.dock.add_window(group.to_vec());
            // Narrow enough that five tabs overflow the tab bar.
            if let Some(state) = shell.dock.get_window_state_mut(window) {
                state.set_size(egui::vec2(180.0, 160.0));
            }
        }
        assert!(dock_has_window(&shell.dock));
        let ctx = egui::Context::default();
        ctx.options_mut(|o| o.warn_on_id_clash = true);
        for frame in 0..4 {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 800.0),
                )),
                time: Some(f64::from(frame) / 60.0),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| shell.ui(ui, app.world_mut(), None));
            output.textures_delta.clear();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-7902 (lead decision on review 3202 #31): every tab bar ends in an overflow button whose
    /// list is the group's hidden tabs; picking one brings it forward and scrolls it into the bar,
    /// and `editor.tab_overflow` reports the same lists.
    #[test]
    fn a_crowded_tab_bar_lists_its_hidden_tabs_and_a_pick_scrolls_one_into_view() {
        let dir = regia_test_utils::unique_temp_dir("editor_shell_tab_overflow");
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        std::fs::write(
            dir.join("Regia.toml"),
            "[project]\nname = \"W\"\nid = \"com.example.w\"\n",
        )
        .expect("manifest");
        let mut app = dirty_level_app("Rock");
        let mut shell = EditorShell::new(None, Some(dir.clone()));
        let tabs = vec![
            Panel::OutputLog,
            Panel::Layers,
            Panel::Problems,
            Panel::SizeMap,
            Panel::Tests,
            Panel::Theme,
            Panel::Extensions,
            Panel::AgentChanges,
            Panel::ContentChunks,
            Panel::ReferenceGraph,
        ];
        shell.dock = egui_dock::DockState::new(tabs.clone());
        let ctx = egui::Context::default();
        let run = |shell: &mut EditorShell, app: &mut regia_app::prelude::App, frame: u32| {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(480.0, 360.0),
                )),
                time: Some(f64::from(frame) / 60.0),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| shell.ui(ui, app.world_mut(), None));
            output.textures_delta.clear();
        };
        for frame in 0..3 {
            run(&mut shell, &mut app, frame);
        }
        let hidden = |app: &mut regia_app::prelude::App| {
            app.world()
                .get_resource::<crate::workspaces::TabStrips>()
                .and_then(|s| s.hidden.get(&(0, 0)).cloned())
                .unwrap_or_default()
        };
        let before = hidden(&mut app);
        assert!(!before.is_empty(), "ten tabs in a 480-point bar: some are hidden");
        assert!(!before.contains(&Panel::OutputLog), "the first tab shows");
        let last = *before.last().expect("a hidden tab");
        assert_eq!(
            last,
            Panel::ReferenceGraph,
            "the last tab is past the strip's end"
        );
        // The agent route lists the same tabs.
        let listed = crate::workspaces::methods()
            .into_iter()
            .find(|(m, _)| m.name == "editor.tab_overflow")
            .map(|(_, h)| h(app.world_mut(), &serde_json::json!({})).expect("tab_overflow"))
            .expect("method");
        let group = &listed["groups"][0];
        assert_eq!(
            group["hidden"].as_array().map(Vec::len),
            Some(before.len()),
            "{listed}"
        );
        assert_eq!(group["tabs"].as_array().map(Vec::len), Some(tabs.len()));
        // The overflow list's pick.
        crate::workspaces::ViewRequests::push(app.world_mut(), crate::workspaces::ViewRequest::Focus(last));
        for frame in 3..6 {
            run(&mut shell, &mut app, frame);
        }
        let after = hidden(&mut app);
        assert!(
            !after.contains(&last),
            "the picked tab scrolled into the bar: {after:?}"
        );
        let path = shell.dock.find_tab(&last).expect("still open");
        let leaf = shell.dock.leaf(path.node_path()).expect("leaf");
        assert_eq!(leaf.tabs[leaf.active.0], last, "and it is the group's active tab");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn names(world: &mut World) -> Vec<String> {
        let mut query = world.query::<&regia_scene::prelude::Name>();
        query.iter(world).map(|n| n.as_str().to_string()).collect()
    }

    /// Audit v033b (High): a clean exit removed the whole autosave folder, so closing the window
    /// while the crash-recovery window still offered an autosave deleted the only copy of the
    /// crashed session's edits. The exit keeps the autosaves still on offer and clears the rest.
    #[test]
    fn exit_keeps_crash_recovery_autosaves_still_on_offer() {
        let dir = regia_test_utils::unique_temp_dir("editor_shell_exit_recovery");
        std::fs::create_dir_all(dir.join("assets/maps")).expect("dir");
        let autosaves = crate::autosave::autosave_dir(&dir);
        std::fs::create_dir_all(&autosaves).expect("dir");
        // A crashed session's untitled level: always offered.
        let crashed = autosaves.join("untitled.level.ron");
        std::fs::write(&crashed, "( format_version: 1, entities: [] )").expect("autosave");
        // A stale autosave: its level was saved after it, so it is not offered and may go.
        let level = dir.join("assets/maps/arena.level.ron");
        std::fs::write(&level, "( format_version: 1, entities: [] )").expect("level");
        let stale = autosaves.join("arena.level.ron");
        std::fs::write(&stale, "( format_version: 1, entities: [] )").expect("autosave");
        let set_mtime = |path: &std::path::Path, secs: u64| {
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .and_then(|f| f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)))
                .expect("mtime");
        };
        set_mtime(&stale, 1_000);
        set_mtime(&level, 2_000);

        let shell = EditorShell::new(None, Some(dir.clone()));
        assert_eq!(shell.pending_recovery(), vec![crashed.clone()]);
        shell.exit_cleanup();
        assert!(crashed.is_file(), "the autosave still on offer survives the exit");
        assert!(!stale.exists(), "an autosave nobody is offered is cleared");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit v033b (High): Restore kept the undo stack of the level it replaced, so Ctrl+Z ran an
    /// old command against the recovered level. Restore starts a fresh, dirty history.
    #[test]
    fn restore_drops_the_replaced_levels_undo_history() {
        let dir = regia_test_utils::unique_temp_dir("editor_shell_restore_undo");
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        // The crashed session autosaved an untitled level with one entity.
        let mut crashed = dirty_level_app("Recovered");
        let open = crashed.world().resource::<OpenLevel>().clone();
        crate::autosave::write_autosave(crashed.world_mut(), &open, &dir).expect("autosave");

        // This session: a fresh level with one edit of its own on the stack.
        let mut app = dirty_level_app("SessionRock");
        let world = app.world_mut();
        let mut shell = EditorShell::new(None, Some(dir.clone()));
        assert_eq!(shell.pending_recovery().len(), 1);
        let toast = shell.restore_first_recovery(world).expect("a candidate");
        assert!(toast.starts_with("restored"), "{toast}");
        assert!(names(world).contains(&"Recovered".to_string()));
        let (depth, dirty) = {
            let undo = world.resource::<UndoStack>();
            (undo.undo_depth(), undo.is_dirty())
        };
        assert_eq!(depth, 0, "the replaced level's commands are gone");
        assert!(dirty, "the restored edits are not saved yet");
        let undone =
            world.resource_scope(|world, mut undo: regia_ecs::prelude::Mut<UndoStack>| undo.undo(world));
        assert!(!undone, "Ctrl+Z has nothing to run against the recovered level");
        assert!(names(world).contains(&"Recovered".to_string()));
        assert!(shell.pending_recovery().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A Restore that cannot open the autosave leaves the offer in place. The next clean exit
    /// keeps files that are still pending, so dropping the offer would delete the only copy.
    #[test]
    fn failed_restore_keeps_the_recovery_offer() {
        let dir = regia_test_utils::unique_temp_dir("editor_shell_restore_fail");
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        let autosave = crate::autosave::autosave_dir(&dir).join("untitled.level.ron");
        std::fs::create_dir_all(autosave.parent().expect("parent")).expect("dir");
        std::fs::write(&autosave, "not a level").expect("autosave");
        let mut app = dirty_level_app("SessionRock");
        let world = app.world_mut();
        let mut shell = EditorShell::new(None, Some(dir.clone()));
        assert_eq!(shell.pending_recovery(), vec![autosave.clone()]);
        let toast = shell.restore_first_recovery(world).expect("a candidate");
        assert!(
            !toast.starts_with("restored"),
            "a bad autosave is not restored: {toast}"
        );
        assert_eq!(
            shell.pending_recovery(),
            vec![autosave.clone()],
            "the offer stays so a later exit does not delete the file"
        );
        assert!(names(world).contains(&"SessionRock".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review sprint-rev-claude-50-k, autosave-recovery finding 4: an agent's
    /// `editor.recovery_discard` and a recovery-window Discard in the same frame. The shell
    /// published its stale copy over the world's, so the document the agent discarded came back
    /// on offer (its file already gone). Both changes stick.
    #[test]
    fn same_frame_agent_and_window_recovery_changes_both_stick() {
        let dir = regia_test_utils::unique_temp_dir("editor_shell_offers_merge");
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        let level = crate::autosave::autosave_dir(&dir).join("untitled.level.ron");
        std::fs::create_dir_all(level.parent().expect("parent")).expect("dir");
        std::fs::write(&level, "( format_version: 1, entities: [] )").expect("autosave");
        let a = crate::doc_autosave::DocKey::new("matgraph", Some(&dir.join("assets/a.matgraph.ron")));
        let b = crate::doc_autosave::DocKey::new("matgraph", Some(&dir.join("assets/b.matgraph.ron")));
        crate::doc_autosave::write_docs(
            &dir,
            &[
                crate::doc_autosave::DocSnapshot {
                    key: a.clone(),
                    bytes: b"a".to_vec(),
                },
                crate::doc_autosave::DocSnapshot {
                    key: b.clone(),
                    bytes: b"b".to_vec(),
                },
            ],
        )
        .expect("docs");
        let mut app = regia_app::prelude::App::new();
        let world = app.world_mut();
        let mut shell = EditorShell::new(None, Some(dir.clone()));
        assert_eq!(shell.pending_recovery(), vec![level.clone()]);
        assert_eq!(shell.recovery_docs.len(), 2);
        shell.sync_offers(world);
        // The agent discards document `a` ...
        let outcome = crate::doc_autosave::discard(world, &[format!("doc:{}", a.id())]);
        assert_eq!(outcome.done.len(), 1, "{outcome:?}");
        // ... and in the same frame the window discards the level.
        let toast = shell.discard_first_recovery().expect("a candidate");
        assert!(toast.starts_with("discarded"), "{toast}");
        shell.sync_offers(world);
        let ids = |docs: &[crate::doc_autosave::DocRecovery]| -> Vec<String> {
            docs.iter().map(|d| d.entry.id.clone()).collect()
        };
        let offers = world.resource::<crate::doc_autosave::RecoveryOffers>().clone();
        assert!(
            offers.levels.is_empty(),
            "the window's discard sticks: {offers:?}"
        );
        assert_eq!(ids(&offers.docs), vec![b.id()], "the agent's discard sticks");
        assert!(shell.pending_recovery().is_empty());
        assert_eq!(ids(&shell.recovery_docs), vec![b.id()]);
        // The next frame changes nothing.
        let revision = offers.revision;
        shell.sync_offers(world);
        assert_eq!(
            world.resource::<crate::doc_autosave::RecoveryOffers>().revision,
            revision
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Discard of an autosave that is already gone (the level was saved first, a second
    /// instance cleaned up, or the user deleted the file) drops the offer: a missing file is
    /// the discarded state. Keeping the offer pinned the crash-recovery window — which has no
    /// close button — for the rest of the session (review sprint-rev-audfix2-b, editor2
    /// finding 2).
    #[test]
    fn discard_of_a_missing_autosave_drops_the_offer() {
        let dir = regia_test_utils::unique_temp_dir("editor_shell_discard_missing");
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        let autosave = crate::autosave::autosave_dir(&dir).join("untitled.level.ron");
        std::fs::create_dir_all(autosave.parent().expect("parent")).expect("dir");
        std::fs::write(&autosave, "( format_version: 1, entities: [] )").expect("autosave");
        let mut shell = EditorShell::new(None, Some(dir.clone()));
        assert_eq!(shell.pending_recovery(), vec![autosave.clone()]);
        // The autosave goes away before Discard.
        std::fs::remove_file(&autosave).expect("remove the autosave");
        let toast = shell.discard_first_recovery().expect("a candidate");
        assert!(toast.starts_with("discarded"), "{toast}");
        assert!(
            shell.pending_recovery().is_empty(),
            "a missing autosave counts as discarded"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The recovery window's Restore and Discard as palette commands (ADR-7307): the command
    /// queues the shell action, the shell's next frame performs it on the first offer.
    #[test]
    fn recovery_commands_restore_and_discard_through_the_shell() {
        let dir = regia_test_utils::unique_temp_dir("editor_shell_recovery_commands");
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        let autosave = crate::autosave::autosave_dir(&dir).join("untitled.level.ron");
        std::fs::create_dir_all(autosave.parent().expect("parent")).expect("dir");
        std::fs::write(&autosave, "( format_version: 1, entities: [] )").expect("autosave");
        let mut shell = EditorShell::new(None, Some(dir.clone()));
        let mut app = regia_app::prelude::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let world = app.world_mut();
        assert_eq!(shell.pending_recovery(), vec![autosave.clone()]);
        let message = crate::command_palette::run(world, "recovery.discard").expect("a command");
        assert!(!message.is_empty());
        shell.apply_layout_requests(world);
        assert!(shell.pending_recovery().is_empty(), "discarded");
        assert!(!autosave.exists(), "the autosave file is gone");
        assert!(shell.toast.starts_with("discarded"), "{}", shell.toast);
        crate::command_palette::run(world, "recovery.restore").expect("a command");
        shell.apply_layout_requests(world);
        assert_eq!(shell.toast, "No autosave to restore");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Material panel's Undo, Redo and Reload from disk as palette commands (ADR-7307).
    #[test]
    fn material_commands_drive_the_material_panel() {
        let dir = regia_test_utils::unique_temp_dir("editor_shell_material_commands");
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("stone.material.ron");
        let material = regia_asset::MaterialAssetRon::default_standard();
        std::fs::write(&path, material.to_ron_string().expect("serialize")).expect("write");
        let mut shell = EditorShell::new(None, None);
        shell.material.open(&path).expect("open");
        shell.material.edit(|m| m.base.roughness = 0.9);
        let mut app = regia_app::prelude::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let world = app.world_mut();
        crate::command_palette::run(world, "material.undo").expect("a command");
        shell.apply_layout_requests(world);
        assert!(!shell.material.dirty, "undone back to the file");
        crate::command_palette::run(world, "material.redo").expect("a command");
        shell.apply_layout_requests(world);
        assert!(
            (shell.material.material.base.roughness - 0.9).abs() < 1e-6,
            "redone"
        );
        crate::command_palette::run(world, "material.reload").expect("a command");
        shell.apply_layout_requests(world);
        assert!(!shell.material.dirty, "reloaded from disk");
        assert_eq!(shell.material.material, material);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Discard removes the offer only after the autosave file is gone. A failed delete leaves
    /// the offer, and the toast carries the IO error.
    #[test]
    fn failed_discard_keeps_the_recovery_offer() {
        let dir = regia_test_utils::unique_temp_dir("editor_shell_discard_fail");
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        let autosave = crate::autosave::autosave_dir(&dir).join("untitled.level.ron");
        std::fs::create_dir_all(autosave.parent().expect("parent")).expect("dir");
        std::fs::write(&autosave, "( format_version: 1, entities: [] )").expect("autosave");
        let mut shell = EditorShell::new(None, Some(dir.clone()));
        assert_eq!(shell.pending_recovery(), vec![autosave.clone()]);
        std::fs::remove_file(&autosave).expect("replace the file");
        std::fs::create_dir(&autosave).expect("a directory is not removable as a file");
        let toast = shell.discard_first_recovery().expect("a candidate");
        assert!(
            toast.starts_with("could not discard"),
            "the toast names the failed discard: {toast}"
        );
        assert_eq!(shell.pending_recovery(), vec![autosave]);
        assert!(!shell.toast.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit v033b (High): the close check looked only at the level stack, so material,
    /// project-settings and UI-document edits were dropped without a prompt. Each counts, and
    /// Save and exit writes it.
    #[test]
    fn close_counts_every_unsaved_document_and_save_writes_them() {
        let dir = regia_test_utils::unique_temp_dir("editor_shell_unsaved_docs");
        std::fs::create_dir_all(dir.join("assets/materials")).expect("dir");
        let mut app = regia_app::prelude::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let open = level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        app.world_mut().init_resource::<UndoStack>();
        let world = app.world_mut();
        let mut shell = EditorShell::new(None, Some(dir.clone()));
        assert!(
            shell.unsaved_documents(world).is_empty(),
            "a clean session closes without asking"
        );

        // The Material panel.
        let material = dir.join("assets/materials/hero.material.ron");
        let text = regia_asset::MaterialAssetRon::default_standard()
            .to_ron_string()
            .expect("ron");
        std::fs::write(&material, text).expect("material");
        shell.material.open(&material).expect("open");
        shell.material.edit(|m| m.base.roughness = 0.125);
        assert_eq!(
            shell.unsaved_documents(world).len(),
            1,
            "{:?}",
            shell.unsaved_documents(world)
        );

        // Project Settings.
        std::fs::write(
            dir.join(regia_core::brand::MANIFEST_FILE),
            "[project]\nname = \"Demo\"\nid = \"com.example.demo\"\nversion = \"0.1.0\"\n",
        )
        .expect("manifest");
        shell.settings.load(&dir).expect("settings");
        shell.settings.set_project_string("name", "Renamed");
        assert_eq!(shell.unsaved_documents(world).len(), 2);

        // A UI document with a path.
        let ui_path = dir.join("assets/menu.ui.ron");
        let mut designer = crate::ui_designer::UiDesigner::default();
        designer.new_document();
        let doc = designer.doc_mut().expect("doc");
        doc.path = Some(ui_path.clone());
        doc.apply(crate::ui_designer::DesignOp::Add {
            parent: Vec::new(),
            index: None,
            node: Box::default(),
        })
        .expect("add");
        world.insert_resource(designer);
        assert_eq!(
            shell.unsaved_documents(world).len(),
            3,
            "{:?}",
            shell.unsaved_documents(world)
        );

        shell.save_other_documents(world).expect("save");
        assert!(shell.unsaved_documents(world).is_empty());
        let saved = std::fs::read_to_string(&material).expect("read");
        assert!(saved.contains("0.125"), "{saved}");
        let manifest = std::fs::read_to_string(dir.join(regia_core::brand::MANIFEST_FILE)).expect("read");
        assert!(manifest.contains("Renamed"), "{manifest}");
        assert!(ui_path.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A material-graph edit is its own document. The close list names the graph file, marking
    /// the level stack saved does not drop it, and Save and exit writes the graph.
    #[test]
    fn a_material_graph_edit_is_not_the_level_and_save_and_exit_writes_it() {
        let dir = regia_test_utils::unique_temp_dir("editor_shell_matgraph_close");
        let path = dir.join("foo.matgraph.ron");
        let mut app = regia_app::prelude::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let open = level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        app.world_mut().init_resource::<UndoStack>();
        {
            let mut editor = crate::matgraph_editor::MatGraphEditor::default();
            editor.new_graph(Some(path.clone()));
            editor.save(None).expect("seed graph");
            app.world_mut().insert_resource(editor);
        }
        let world = app.world_mut();
        crate::matgraph_editor::edit(world, "add", None, |doc| {
            doc.add_node(Some("c"), regia_matgraph::Node::Float(0.5), None)
                .map(|_| ())
        })
        .expect("edit");
        let mut shell = EditorShell::new(None, Some(dir.clone()));
        let listed = shell.unsaved_documents(world);
        assert!(
            listed.iter().any(|doc| doc.contains("foo.matgraph.ron")),
            "{listed:?}"
        );
        assert!(
            !listed.iter().any(|doc| doc == "the level"),
            "a graph edit is not the level: {listed:?}"
        );
        assert!(world.resource::<crate::matgraph_editor::MatGraphEditor>().dirty());
        assert!(!world.resource::<UndoStack>().is_dirty());
        let before = std::fs::read(&path).expect("seed bytes");
        world.resource_mut::<UndoStack>().mark_saved();
        let listed = shell.unsaved_documents(world);
        assert!(
            listed.iter().any(|doc| doc.contains("foo.matgraph.ron")),
            "mark_saved does not clear the graph: {listed:?}"
        );
        assert!(world.resource::<crate::matgraph_editor::MatGraphEditor>().dirty());
        shell.save_other_documents(world).expect("save and exit");
        let after = std::fs::read(&path).expect("saved bytes");
        assert_ne!(before, after, "the graph file changed");
        assert!(
            shell.unsaved_documents(world).is_empty(),
            "{:?}",
            shell.unsaved_documents(world)
        );
        assert!(!world.resource::<crate::matgraph_editor::MatGraphEditor>().dirty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit v033b (High): `quit.request` exited without writing an autosave, so up to five
    /// minutes of level edits were gone. A dirty level is autosaved at the quit and kept.
    #[test]
    fn agent_quit_autosaves_a_dirty_level_first() {
        let dir = regia_test_utils::unique_temp_dir("editor_shell_agent_quit");
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        let mut app = dirty_level_app("QuitRock");
        let world = app.world_mut();
        // The default 5-minute interval: the tick has not run.
        let mut shell = EditorShell::new(None, Some(dir.clone()));
        assert!(!shell.prepare_agent_quit(world), "the level is dirty");
        let autosave = crate::autosave::autosave_dir(&dir).join("untitled.level.ron");
        let text = std::fs::read_to_string(&autosave).expect("the quit wrote an autosave");
        assert!(text.contains("QuitRock"), "{text}");

        // A clean level quits cleanly and clears the session's autosaves.
        world.resource_mut::<UndoStack>().mark_saved();
        assert!(shell.prepare_agent_quit(world));
        assert!(!autosave.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rc1 editor-undo finding 5 (ADR-7412): `quit.request` wrote its autosave over a
    /// crash-recovery autosave the recovery window still offered (the 5-minute tick refuses
    /// that write). The crashed session's only copy must survive the quit.
    #[test]
    fn agent_quit_never_overwrites_a_pending_recovery_autosave() {
        let dir = regia_test_utils::unique_temp_dir("editor_shell_agent_quit_pending");
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        let crashed = crate::autosave::autosave_dir(&dir).join("untitled.level.ron");
        std::fs::create_dir_all(crashed.parent().expect("parent")).expect("dir");
        let crashed_text = "( format_version: 1, entities: [] ) // the crashed session";
        std::fs::write(&crashed, crashed_text).expect("autosave");
        let mut app = dirty_level_app("QuitRock");
        let world = app.world_mut();
        let mut shell = EditorShell::new(None, Some(dir.clone()));
        assert_eq!(shell.pending_recovery(), vec![crashed.clone()]);

        assert!(!shell.prepare_agent_quit(world), "the level is dirty");
        assert_eq!(
            std::fs::read_to_string(&crashed).expect("the crashed autosave is still there"),
            crashed_text,
            "the quit did not write over the pending recovery file"
        );
        // rc2 review (editor-undo finding 2): this session's edits are not dropped either; they
        // are written beside the crashed copy, and the next launch offers both.
        let beside = crate::autosave::autosave_dir(&dir).join("untitled~quit.level.ron");
        let text = std::fs::read_to_string(&beside).expect("the quit autosaved beside the pending file");
        assert!(text.contains("QuitRock"), "{text}");
        let mut offered: Vec<std::path::PathBuf> = crate::autosave::recovery_candidates(&dir)
            .into_iter()
            .map(|candidate| candidate.autosave)
            .collect();
        offered.sort();
        assert_eq!(
            offered,
            vec![crashed.clone(), beside.clone()],
            "sorted: `.` before `~`"
        );
        // A second quit with both still on offer takes the next free name.
        let mut shell = EditorShell::new(None, Some(dir.clone()));
        assert!(!shell.prepare_agent_quit(world), "the level is still dirty");
        let third = crate::autosave::autosave_dir(&dir).join("untitled~quit2.level.ron");
        assert!(third.is_file(), "the second quit wrote {}", third.display());
        assert_eq!(
            std::fs::read_to_string(&crashed).expect("crashed copy"),
            crashed_text
        );
        assert!(
            std::fs::read_to_string(&beside)
                .expect("first quit copy")
                .contains("QuitRock")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// File → Save runs the terrain save too: a sculpt reaches the cooked file, a hole and painted
    /// weights their files next to the level, and the autosave's level-only path stays apart.
    #[test]
    fn save_level_saves_what_the_terrain_tool_edited() {
        use crate::terrain_tool::{
            TerrainCookTarget, TerrainTool, begin_stroke, end_stroke, pump, target_terrain,
        };
        use regia_app::prelude::App;
        use regia_core::math::Vec2;
        use regia_terrain::{
            CdlodConfig, LayerWeightMap, Terrain, TerrainEdits, TerrainLayers, TerrainMap, TerrainPaint,
        };

        let dir = regia_test_utils::unique_temp_dir("editor_shell_terrain_save");
        let level = dir.join("arena.level.ron");
        let cache = dir.join("cache");
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let mut open = level_io::new_level(app.world_mut());
        open.path = Some(level.clone());
        app.world_mut().insert_resource(open);

        let n = 129u32;
        let heights = (0..n * n).map(|i| 20.0 + (i % n) as f32 * 0.01).collect();
        let map = TerrainMap::from_heights(n, n, heights, 128.0, 100.0, 0.0).expect("map");
        let terrain = Terrain::new(
            &map.to_heightfield(),
            128.0,
            100.0,
            0.0,
            CdlodConfig::default(),
            4,
            TerrainLayers::default(),
        )
        .expect("terrain");
        let world = app.world_mut();
        let e = world
            .spawn((
                terrain,
                TerrainPaint(LayerWeightMap::new(n, n, 1).expect("paint")),
                TerrainCookTarget {
                    map_id: regia_asset::AssetId::new_random(),
                    cache_dir: cache.clone(),
                },
            ))
            .id();
        assert_eq!(target_terrain(world), Some(e));
        world.init_resource::<TerrainTool>();
        let class = regia_terrain::ScatterClass {
            name: "trees".into(),
            meshes: vec![regia_terrain::ScatterMeshRef {
                mesh: "tree".into(),
                weight: 1.0,
                min_scale: 1.0,
                max_scale: 1.0,
                scale_bias: 1.0,
            }],
            ..regia_terrain::ScatterClass::default()
        };
        world.insert_resource(regia_terrain::ScatterPaintSet(std::sync::Arc::new(
            regia_terrain::ScatterSet {
                classes: vec![class],
                ..regia_terrain::ScatterSet::default()
            },
        )));
        world.resource_mut::<TerrainTool>().scatter_density = 1.0;
        // Scatter first: the hole below would take the whole brush (classes avoid holes).
        for mode in [9, 1, 5, 8] {
            world.resource_mut::<TerrainTool>().select_mode(mode);
            assert!(begin_stroke(world, e, Vec2::ZERO, false));
            end_stroke(world);
            pump(world, true);
        }

        let mut shell = EditorShell::new(None, Some(dir.clone()));
        let toast = shell.save_level(world);
        assert!(toast.starts_with("Saved"), "{toast}");
        assert!(level.is_file());
        assert!(dir.join("arena.holes.ron").is_file(), "the hole is saved");
        assert!(dir.join("arena.weights.bin").is_file(), "the paint is saved");
        assert!(
            dir.join("arena.scatterpaint.bin").is_file(),
            "the painted instances are saved"
        );
        let cooked = std::fs::read_dir(&cache).map_or(0, |files| files.count());
        assert!(cooked > 0, "the sculpt is re-cooked");
        let edits = world.get::<TerrainEdits>(e).expect("edits");
        assert!(edits.unsaved_tiles().is_empty());
        assert!(!edits.unsaved_paint());
        assert!(!edits.unsaved_scatter());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `specs/19` § 4: Alt+P plays like the toolbar (only while stopped, never from a text field),
    /// and Escape stops a running session instead of closing the editor.
    #[test]
    fn alt_p_plays_and_escape_stops_play_instead_of_closing() {
        fn frame(ctx: &egui::Context, world: &mut World, active: bool, events: Vec<egui::Event>) {
            let raw = egui::RawInput {
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| play_shortcuts(ui.ctx(), world, active));
            output.textures_delta.clear();
        }
        fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
            egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }
        }
        let queued = |world: &World| {
            world
                .get_resource::<crate::pie::PieRequests>()
                .map(|requests| requests.queue.clone())
                .unwrap_or_default()
        };
        let ctx = egui::Context::default();
        let mut world = World::new();

        frame(
            &ctx,
            &mut world,
            false,
            vec![key(egui::Key::P, egui::Modifiers::NONE)],
        );
        assert!(queued(&world).is_empty(), "a bare P is not the play shortcut");
        frame(
            &ctx,
            &mut world,
            true,
            vec![key(egui::Key::P, egui::Modifiers::ALT)],
        );
        assert!(
            queued(&world).is_empty(),
            "Alt+P does not restart a running session"
        );
        frame(
            &ctx,
            &mut world,
            false,
            vec![key(egui::Key::P, egui::Modifiers::ALT)],
        );
        assert_eq!(queued(&world), vec![crate::pie::PieRequest::Play]);

        assert_eq!(escape_action(true, false), EscapeAction::StopPlay);
        assert_eq!(escape_action(false, false), EscapeAction::Close);
        assert_eq!(escape_action(true, true), EscapeAction::Ignore);
        assert_eq!(escape_action(false, true), EscapeAction::Ignore);
    }

    /// Ctrl+Z / Ctrl+Y / Ctrl+Shift+Z step the shared stack, and a pointer release ends a drag:
    /// two drags on the same entity are two entries.
    #[test]
    fn edit_shortcuts_step_the_stack_and_a_release_seals_it() {
        use crate::command::{EditorCommand as _, SetTransform};
        use regia_core::math::Vec3;
        use regia_scene::prelude::Transform;

        fn frame(ctx: &egui::Context, world: &mut World, events: Vec<egui::Event>) {
            let raw = egui::RawInput {
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| edit_shortcuts(ui.ctx(), world, false));
            output.textures_delta.clear();
        }
        fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
            egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }
        }
        fn drag(world: &mut World, entity: Entity, x: f32) {
            let mut command =
                SetTransform::capture(world, entity, Transform::from_translation(Vec3::new(x, 0.0, 0.0)));
            command.apply(world);
            world
                .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                .push(Box::new(command));
        }
        fn x(world: &World, entity: Entity) -> f32 {
            world
                .get::<Transform>(entity)
                .map_or(f32::NAN, |t| t.translation.x)
        }
        fn depth(world: &World) -> usize {
            world.resource::<UndoStack>().undo_depth()
        }

        let mut world = World::new();
        world.insert_resource(UndoStack::new());
        let entity = world.spawn(Transform::IDENTITY).id();
        let ctx = egui::Context::default();
        let command = egui::Modifiers::COMMAND;
        let pos = egui::Pos2::new(10.0, 10.0);
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };

        drag(&mut world, entity, 1.0);
        drag(&mut world, entity, 2.0);
        assert_eq!(depth(&world), 1, "one drag");
        frame(
            &ctx,
            &mut world,
            vec![egui::Event::PointerMoved(pos), button(true)],
        );
        frame(&ctx, &mut world, vec![button(false)]);
        drag(&mut world, entity, 3.0);
        assert_eq!(depth(&world), 2, "the release ended the first drag");

        frame(&ctx, &mut world, vec![key(egui::Key::Z, command)]);
        assert_eq!(x(&world, entity), 2.0, "Ctrl+Z undid the second drag");
        frame(&ctx, &mut world, vec![key(egui::Key::Y, command)]);
        assert_eq!(x(&world, entity), 3.0, "Ctrl+Y redid it");
        frame(&ctx, &mut world, vec![key(egui::Key::Z, command)]);
        frame(&ctx, &mut world, vec![key(egui::Key::Z, command)]);
        assert_eq!(x(&world, entity), 0.0, "both drags undone");
        frame(
            &ctx,
            &mut world,
            vec![key(egui::Key::Z, command | egui::Modifiers::SHIFT)],
        );
        assert_eq!(x(&world, entity), 2.0, "Ctrl+Shift+Z redoes, it does not undo");
    }

    /// Review w8 finding 4 (ADR-7496): `editor.menu_list` answers `crate::menus::menu_tree`, and
    /// the shell draws the menus. One shell pass, with every menu drawn open, must draw exactly
    /// the tree's items in the tree's order: an item listed but not drawn (Play ▸ Compile was), a
    /// drawn item the list misses (one Create ▸ Shape row stood for every kind), or a new menu
    /// action without a tree row fails here.
    #[test]
    fn the_drawn_menus_are_the_menu_list() {
        // A project, so the shell draws the dock and the menu bar (not the project browser).
        let dir = regia_test_utils::unique_temp_dir("editor_shell_menu_drift");
        std::fs::create_dir_all(dir.join("assets")).expect("assets");
        std::fs::write(dir.join("Regia.toml"), "[project]\nname = \"M\"\n").expect("manifest");
        let mut app = regia_app::prelude::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let open = level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
        // One recent level, so File ▸ Recent draws its one entry (the tree lists the submenu once).
        app.world_mut()
            .insert_resource(crate::viewport_ops::RecentLevels {
                paths: vec![std::path::PathBuf::from("assets/maps/a.level.ron")],
            });
        let mut shell = EditorShell::new(None, Some(dir.clone()));
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1600.0, 1000.0),
            )),
            ..Default::default()
        };
        let mut drawn = Vec::new();
        let mut output = ctx.run_ui(raw, |ui| {
            // egui may run a pass twice; the last pass is the drawn one.
            let ((), items) = crate::agent_parity::record_menus(|| shell.ui(ui, app.world_mut(), None));
            drawn = items;
        });
        output.textures_delta.clear();
        let drawn: Vec<(String, &str)> = drawn
            .iter()
            .map(|item| {
                // File ▸ Recent lists the recent levels; the tree names the submenu once.
                let path = if item.action == crate::agent_parity::FILE_RECENT.id {
                    item.menu.clone()
                } else {
                    format!("{}/{}", item.menu, item.text)
                };
                (path, item.action)
            })
            .collect();
        let listed: Vec<(String, &str)> = crate::menus::menu_tree()
            .iter()
            .map(|item| (item.path(), item.action.id))
            .collect();
        let missing: Vec<&(String, &str)> = listed.iter().filter(|i| !drawn.contains(i)).collect();
        let unlisted: Vec<&(String, &str)> = drawn.iter().filter(|i| !listed.contains(i)).collect();
        assert!(
            missing.is_empty() && unlisted.is_empty(),
            "menu_list items no menu draws: {missing:?}\ndrawn items menu_list misses: {unlisted:?}"
        );
        assert_eq!(drawn, listed, "the same items in the same order");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// sprint-w13-ui-live-bugs (3): the status bar's Unsaved indicator is File > Save all
    /// (`status.save_all`, ADR-7822): it opens the Save all dialog (a click used to save only the
    /// level). The keyboard path: Tab to the indicator, Enter.
    #[test]
    fn the_unsaved_indicator_opens_the_save_all_dialog() {
        use crate::save_all_dialog::{SaveAllDialog, SavePurpose};
        let key = |key: egui::Key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let mut opened_after = None;
        for tabs in 1..=16 {
            let mut app = regia_app::prelude::App::new();
            app.add_plugins(regia_scene::ScenePlugin);
            let world = app.world_mut();
            world.insert_resource(UndoStack::new());
            world.resource_mut::<UndoStack>().mark_dirty();
            let mut shell = EditorShell::new(None, None);
            let ctx = egui::Context::default();
            let mut frames: Vec<Vec<egui::Event>> = vec![Vec::new()];
            frames.extend((0..tabs).map(|_| vec![key(egui::Key::Tab)]));
            frames.push(vec![key(egui::Key::Enter)]);
            frames.push(Vec::new());
            for events in frames {
                let raw = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1600.0, 600.0),
                    )),
                    events,
                    ..Default::default()
                };
                let mut output = ctx.run_ui(raw, |ui| {
                    shell.status_bar(&ctx, ui, world);
                });
                output.textures_delta.clear();
            }
            let opened = world
                .get_resource::<SaveAllDialog>()
                .is_some_and(|dialog| dialog.purpose == Some(SavePurpose::SaveAll));
            if opened {
                opened_after = Some(tabs);
                assert!(
                    world.resource::<UndoStack>().is_dirty(),
                    "the click only opens the dialog; nothing is saved behind it"
                );
                break;
            }
        }
        assert!(
            opened_after.is_some(),
            "Tab + Enter on the Unsaved indicator opens the Save all dialog"
        );
    }

    /// sprint-w12-rev-g10 row 4: on close, **Save selected** with a dirty material that has no file
    /// (the dialog cannot save it) keeps the editor open with the dialog up; **Don't save** exits.
    #[test]
    fn save_selected_on_close_keeps_an_unsavable_material_open() {
        use crate::autosave::CloseDecision;
        use crate::save_all_dialog::{SaveAllDialog, SaveDecision, SavePurpose};
        let mut app = regia_app::prelude::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let world = app.world_mut();
        let mut shell = EditorShell::new(None, None);
        shell.material.dirty = true;
        assert!(shell.material.path.is_none(), "an untitled material");
        let decide = |shell: &mut EditorShell, world: &mut World, choice: SaveDecision| {
            let extra = shell.shell_dirty_rows();
            SaveAllDialog::choose(world, choice, &extra);
            let (purpose, decision) = world
                .resource_mut::<SaveAllDialog>()
                .take_decision()
                .unwrap_or_else(|| panic!("the dialog decided"));
            shell.apply_save_all_decision(world, &purpose, decision);
            shell.take_close_decision()
        };
        crate::save_all_dialog::open_dialog(world, SavePurpose::Close);
        let close = decide(&mut shell, world, SaveDecision::Saved { extra: Vec::new() });
        assert_ne!(
            close,
            Some(CloseDecision::DiscardAndExit),
            "Save selected must not exit and drop the material's edits"
        );
        assert!(shell.material.dirty, "the edits are still there");
        let dialog = world.resource::<SaveAllDialog>();
        assert_eq!(dialog.purpose, Some(SavePurpose::Close), "the dialog stays up");
        assert!(
            dialog
                .errors
                .iter()
                .any(|e| e.starts_with("the material: not saved")),
            "{:?}",
            dialog.errors
        );
        // Don't save is the discard.
        let close = decide(&mut shell, world, SaveDecision::DontSave);
        assert_eq!(close, Some(CloseDecision::DiscardAndExit));
    }

    /// P-147: `editor.save_all` lists the shell's Material panel row and saves it: the save is
    /// queued and the shell's next frame writes the material.
    #[test]
    fn editor_save_all_lists_and_saves_the_shell_rows() {
        let root = regia_test_utils::unique_temp_dir("shell_save_all_rows");
        std::fs::create_dir_all(root.join("assets/materials")).expect("dir");
        let file = root.join("assets/materials/stone.material.ron");
        let mut app = regia_app::prelude::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let world = app.world_mut();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let mut shell = EditorShell::new(None, Some(root.clone()));
        shell.material.path = Some(file.clone());
        shell.material.dirty = true;
        shell.sync_save_all_rows(world);
        let (_, save_all) = crate::save_all_dialog::methods()
            .into_iter()
            .next()
            .expect("editor.save_all");
        let listed = save_all(world, &serde_json::json!({ "list": true })).expect("list");
        let row = listed["documents"]
            .as_array()
            .and_then(|docs| docs.iter().find(|d| d["id"] == "shell:material").cloned())
            .unwrap_or_else(|| panic!("the material row is listed: {listed}"));
        assert_eq!(row["can_save"], true, "{row}");
        let out = save_all(world, &serde_json::json!({ "ids": ["shell:material"] })).expect("save");
        assert_eq!(out["queued"], serde_json::json!(["shell:material"]), "{out}");
        assert!(!file.exists(), "queued: the shell writes it on its next frame");
        shell.sync_save_all_rows(world);
        assert!(file.is_file(), "the shell's frame saved the material");
        assert!(!shell.material.dirty);
        let out = save_all(world, &serde_json::json!({ "list": true })).expect("list");
        assert!(
            !out["documents"]
                .as_array()
                .is_some_and(|docs| docs.iter().any(|d| d["id"] == "shell:material")),
            "{out}"
        );
        let after = save_all(world, &serde_json::json!({ "ids": [] })).expect("results");
        assert_eq!(
            after["shell_results"],
            serde_json::json!([{ "id": "shell:material", "error": null }]),
            "{after}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
