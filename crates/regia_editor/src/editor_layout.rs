// SPDX-License-Identifier: MPL-2.0
//! Dock layout persistence (`docs/specs/19-editor.md` § 2, ADR-2321): the editor's `egui_dock` tree
//! saved per project and per user, restored when a project opens, and "Reset layout".
//!
//! A [`LayoutDoc`] is the dock's main surface as a plain tree of [`LayoutNode`]s naming panels by
//! stable ids ([`Panel::id`]), independent of `egui_dock`'s internal storage, so the file survives
//! dock upgrades:
//!
//! ```ron
//! (
//!     format_version: 2,
//!     root: Split(axis: Horizontal, fraction: 0.17,
//!         first: Tabs(tabs: ["outliner"], active: 0),
//!         second: Tabs(tabs: ["viewport", "content"], active: 0)),
//!     known_panels: ["viewport", "outliner", "content"],
//!     floating: [(tabs: ["problems"], active: 0, rect: Some((40.0, 60.0, 480.0, 320.0)))],
//! )
//! ```
//!
//! Files: `<project>/.regia/editor_layout.ron` (the project's layout) and
//! `<user config>/regia/editor_layout.ron` (the user's default for projects without one). A project
//! opens with its own layout, else the user's default, else the built-in [`default_dock`]
//! (Preferences ▸ Layout ▸ Restore saved layout turns this off).
//!
//! [`LayoutDoc::to_dock`] normalizes what it reads: unknown ids, extension panels (they come back
//! when their extension registers them) and repeated panels are dropped, empty tab groups collapse
//! their split, fractions are clamped, and a panel that the default layout has but that did not
//! exist when the file was saved (`known_panels`) is added to the bottom tab group, so a panel a
//! newer build adds is never lost behind an old file. Nesting deeper than [`MAX_DEPTH`] is refused
//! (the dock stores its tree as a full binary heap).
//!
//! Version 2 (ADR-2771) adds `floating`: the dock's floating windows, each as a flat tab list with its
//! rectangle. A version 1 file reads as version 2 with no floating windows (the migration step).
//! A window that no longer fits the screen is moved back on it ([`clamp_window`]). A floating
//! window holding a split is saved as one tab group of its panels.
//!
//! Version 3 (ADR-8105 (a), P-003) adds panel instances: a tab id may be `<panel>#<n>` (`n` 1..=7)
//! for the Outliner, Content Browser and Inspector, and `instances` holds each instance's own state
//! ([`PanelInstanceState`]) by tab id. Instance 0 is the bare id, so a version 1 or 2 file reads as
//! version 3 with instance 0 of each panel (the migration is the version number only).
//!
//! The agent surface is `editor.layout_get`, `editor.layout_save`, `editor.layout_apply` and
//! `editor.layout_reset` (`book/agents/editor-preferences.md`).

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use egui_dock::{DockState, Node, NodeIndex, TabIndex};
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::shell::Panel;

/// Current version of `editor_layout.ron`.
pub const EDITOR_LAYOUT_FORMAT_VERSION: u32 = 3;
/// The deepest split nesting a layout may have.
pub const MAX_DEPTH: usize = 10;
/// The most tabs a layout may name.
pub const MAX_TABS: usize = 512;
/// The project layout file, relative to the project root.
pub const PROJECT_LAYOUT_FILE: &str = ".regia/editor_layout.ron";

/// Every built-in panel (extension panels are added at run time and not listed).
pub const BUILT_IN_PANELS: &[Panel] = &[
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
    Panel::Preferences,
    Panel::Terrain,
    Panel::BehaviorTree,
    Panel::MotionMatching,
    Panel::UiInspector,
    Panel::WorldSettings,
    Panel::VfxEditor,
    Panel::StateTreeEditor,
    Panel::Video,
    Panel::Conversation,
    Panel::Localization,
    Panel::Cloth,
    Panel::LookDev,
    Panel::DataAsset,
    Panel::VehicleTuning,
    Panel::LogicGraph,
    Panel::SmartObjectDef,
    Panel::Biome,
    Panel::BulkEdit,
    Panel::ProjectHealth,
    Panel::Retarget,
    Panel::WorldMap,
    Panel::UndoHistory,
    Panel::SelectedDebug,
    Panel::VisualLog,
    Panel::Notes,
    Panel::Activity,
    Panel::Agent,
    Panel::MatchRules,
];

impl Panel {
    /// The stable id the layout file and the agent use (`ext:<id>` for an extension panel).
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Viewport => "viewport",
            Self::Outliner => "outliner",
            Self::Inspector => "inspector",
            Self::Content => "content",
            Self::OutputLog => "output_log",
            Self::Material => "material",
            Self::ProjectSettings => "project_settings",
            Self::AssetViewer => "asset_viewer",
            Self::AnimGraph => "anim_graph",
            Self::AnimClip => "anim_clip",
            Self::AnimSkeleton => "anim_skeleton",
            Self::Kit => "kit",
            Self::Bake => "bake",
            Self::BakeRunner => "bake_runner",
            Self::Lightmaps => "lightmaps",
            Self::Theme => "theme",
            Self::Shots => "shots",
            Self::Net => "net",
            Self::Tool => "tools",
            Self::Mode2d => "mode2d",
            Self::DccFeed => "dcc_feed",
            Self::ProcGraph => "proc_graph",
            Self::ContentChunks => "content_chunks",
            Self::Extensions => "extensions",
            Self::ExtensionPanel(..) => "ext",
            Self::SeqCurves => "seq_curves",
            Self::UiDesigner => "ui_designer",
            Self::UiAnim => "ui_anim",
            Self::Fracture => "fracture",
            Self::Tables => "tables",
            Self::Abilities => "abilities",
            Self::Montage => "montage",
            Self::Ragdoll => "ragdoll",
            Self::Problems => "problems",
            Self::ReferenceGraph => "reference_graph",
            Self::SizeMap => "size_map",
            Self::Layers => "layers",
            Self::Tests => "tests",
            Self::StateTrees => "state_trees",
            Self::SmartObjects => "smart_objects",
            Self::Cameras => "cameras",
            Self::MaterialGraph => "material_graph",
            Self::AudioMixer => "audio_mixer",
            Self::CueEditor => "cue_editor",
            Self::AgentChanges => "agent_changes",
            Self::Agent => "agent",
            Self::Preferences => "preferences",
            Self::Terrain => "terrain",
            Self::BehaviorTree => "behavior_tree",
            Self::MotionMatching => "motion_matching",
            Self::UiInspector => "ui_inspector",
            Self::WorldSettings => "world_settings",
            Self::VfxEditor => "vfx_editor",
            Self::StateTreeEditor => "state_tree_editor",
            Self::Video => "video",
            Self::Conversation => "conversation",
            Self::Localization => "localization",
            Self::Cloth => "cloth",
            Self::LookDev => "lookdev",
            Self::DataAsset => "data_asset",
            Self::VehicleTuning => "vehicle_tuning",
            Self::LogicGraph => "logic_graph",
            Self::SmartObjectDef => "smart_object_def",
            Self::Biome => "biome",
            Self::BulkEdit => "bulk_edit",
            Self::ProjectHealth => "project_health",
            Self::Retarget => "retarget",
            Self::WorldMap => "world_map",
            Self::UndoHistory => "undo_history",
            Self::SelectedDebug => "selected_debug",
            Self::VisualLog => "visual_log",
            Self::Notes => "notes",
            Self::Activity => "activity",
            Self::MatchRules => "match_rules",
            Self::GameView(_) => "game_view",
            Self::Instance(kind, n) => Self::instance_id(kind, n),
            Self::UtilityPanel(..) => "utility_panel",
        }
    }

    /// The built-in panel with `id` (`None` for unknown ids and extension panels). An instance id
    /// `<panel>#<n>` (version 3, ADR-8105 (a)) names instance `n` (`1..=7`, no leading zero) of the
    /// Outliner, Content Browser or Inspector; any other suffix is unknown.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        if let Some((base, n)) = id.split_once('#') {
            let kind = crate::shell::InstancedPanel::of(Self::from_id(base)?)?;
            // One digit 1..=7: no sign, no leading zero, nothing after it.
            let n = match n.as_bytes() {
                [digit @ b'1'..=b'7'] => digit - b'0',
                _ => return None,
            };
            return kind.instance(n);
        }
        BUILT_IN_PANELS.iter().copied().find(|p| p.id() == id)
    }
}

/// The panels of the bottom tab group of the default layout, in order.
const DEFAULT_BOTTOM: &[Panel] = &[
    Panel::Content,
    Panel::OutputLog,
    Panel::Material,
    Panel::AssetViewer,
    Panel::ProjectSettings,
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
    Panel::Cloth,
    Panel::LookDev,
    Panel::VehicleTuning,
    Panel::LogicGraph,
    Panel::Biome,
    Panel::ProjectHealth,
    Panel::Retarget,
    Panel::WorldMap,
    Panel::SelectedDebug,
    Panel::VisualLog,
];

/// The built-in layout: `[outliner | viewport over bottom(content, log, …) | inspector]`.
/// Preferences and World Settings are not in it; the Edit menu opens them as tabs.
#[must_use]
pub fn default_dock() -> DockState<Panel> {
    let mut dock = DockState::new(vec![Panel::Viewport]);
    let tree = dock.main_surface_mut();
    // The splits return [old, new] and `fraction` is the left/top child's share.
    let [viewport, _outliner] = tree.split_left(NodeIndex::root(), 0.17, vec![Panel::Outliner]);
    let [viewport, _inspector] = tree.split_right(viewport, 0.76, vec![Panel::Inspector]);
    let [_, _bottom] = tree.split_below(viewport, 0.68, DEFAULT_BOTTOM.to_vec());
    dock
}

/// How a split lays out its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitAxis {
    /// Side by side: `first` on the left.
    Horizontal,
    /// Stacked: `first` on top.
    Vertical,
}

/// One node of a saved layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LayoutNode {
    /// A tab group.
    Tabs {
        /// Panel ids, in tab order.
        tabs: Vec<String>,
        /// The shown tab's index.
        #[serde(default)]
        active: usize,
    },
    /// Two children sharing the space.
    Split {
        /// Side by side or stacked.
        axis: SplitAxis,
        /// The first child's share, 0..1.
        fraction: f32,
        /// Left or top.
        first: Box<LayoutNode>,
        /// Right or bottom.
        second: Box<LayoutNode>,
    },
}

/// A saved dock layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutDoc {
    /// [`EDITOR_LAYOUT_FORMAT_VERSION`] (a missing field reads as the current version).
    #[serde(default = "crate::editor_prefs::version_missing")]
    pub format_version: u32,
    /// The tree.
    pub root: LayoutNode,
    /// Every built-in panel id the saving editor had (new panels are added on restore).
    #[serde(default)]
    pub known_panels: Vec<String>,
    /// The floating dock windows (version 2; a version 1 file has none).
    #[serde(default)]
    pub floating: Vec<FloatingWindow>,
    /// Each panel instance's own state by tab id (`outliner`, `outliner#1`, …; version 3, ADR-8105
    /// (a)). A key naming no tab of the layout is dropped on save.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub instances: std::collections::BTreeMap<String, PanelInstanceState>,
}

/// One panel instance's saved state (version 3, ADR-8105 (a)). Every field defaults, so a
/// panel's entry names only what differs from a fresh instance.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PanelInstanceState {
    /// The search text (Outliner, Content Browser; empty: no search).
    pub filter: String,
    /// The shown folder relative to the project root, as the Content Browser keeps it
    /// (`assets/textures`; empty: `assets/` itself).
    pub folder: String,
    /// The shown columns by wire name (Outliner; empty: the panel's default columns).
    pub columns: Vec<String>,
    /// The locked subject as `guid:<32 hex>` (Inspector; `None`: follows the selection). A guid
    /// the open level does not have restores unlocked.
    pub locked: Option<String>,
}

impl PanelInstanceState {
    /// Whether the state is a fresh instance's (nothing worth saving).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The most floating windows a layout may name.
pub const MAX_FLOATING: usize = 32;
/// Smallest floating window, points.
pub const MIN_WINDOW_SIZE: [f32; 2] = [160.0, 100.0];
/// The window's size when the file gives none, points.
pub const DEFAULT_WINDOW_SIZE: [f32; 2] = [480.0, 320.0];

/// One saved floating window: a flat group of panels and where it sat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FloatingWindow {
    /// Panel ids, in tab order.
    pub tabs: Vec<String>,
    /// The shown tab's index.
    #[serde(default)]
    pub active: usize,
    /// `(x, y, width, height)` in points, when the window had been shown.
    #[serde(default)]
    pub rect: Option<(f32, f32, f32, f32)>,
}

/// Moves and shrinks a window rectangle so it lies on a screen of `screen` points: the size is
/// kept within [`MIN_WINDOW_SIZE`] and the screen (a non-finite size becomes
/// [`DEFAULT_WINDOW_SIZE`]), and the position is pulled back so the whole window is visible (a
/// non-finite one goes to the top-left). Returns `(position, size)`. A screen smaller than the
/// minimum size is treated as that minimum.
#[must_use]
pub fn clamp_window(pos: [f32; 2], size: [f32; 2], screen: [f32; 2]) -> ([f32; 2], [f32; 2]) {
    let (x, w) = clamp_axis(
        pos[0],
        size[0],
        screen[0],
        MIN_WINDOW_SIZE[0],
        DEFAULT_WINDOW_SIZE[0],
    );
    let (y, h) = clamp_axis(
        pos[1],
        size[1],
        screen[1],
        MIN_WINDOW_SIZE[1],
        DEFAULT_WINDOW_SIZE[1],
    );
    ([x, y], [w, h])
}

/// One axis of [`clamp_window`]: `(position, size)`.
fn clamp_axis(pos: f32, size: f32, screen: f32, min: f32, default: f32) -> (f32, f32) {
    let avail = if screen.is_finite() { screen.max(min) } else { min };
    let wanted = if size.is_finite() { size } else { default };
    let size = wanted.clamp(min, avail);
    let at = if pos.is_finite() { pos } else { 0.0 };
    (at.clamp(0.0, avail - size), size)
}

/// A restored floating window's first panel and its saved rectangle `(x, y, width, height)`.
type RestoredRect = (Panel, (f32, f32, f32, f32));

std::thread_local! {
    /// Saved rectangles of the floating windows the last restores made, by the window's first
    /// panel (ADR-7821; at most one per panel).
    static RESTORED_RECTS: std::cell::RefCell<Vec<RestoredRect>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn remember_restored_rect(first: Panel, rect: (f32, f32, f32, f32)) {
    RESTORED_RECTS.with(|rects| {
        let mut rects = rects.borrow_mut();
        rects.retain(|(panel, _)| *panel != first);
        rects.push((first, rect));
    });
}

/// The saved rectangle `(x, y, width, height)` (points, relative to the main window) of the
/// floating window a layout restore made with `first` as its first panel, taken once: an OS
/// window opens there (ADR-7821). `None` when no restore placed such a window.
#[must_use]
pub fn take_restored_rect(first: Panel) -> Option<(f32, f32, f32, f32)> {
    RESTORED_RECTS.with(|rects| {
        let mut rects = rects.borrow_mut();
        let at = rects.iter().position(|(panel, _)| *panel == first)?;
        Some(rects.swap_remove(at).1)
    })
}

/// Where the floating windows are, as `(x, y, width, height)` points by surface index. The dock
/// state does not know (its window rectangle is never filled in): egui keeps the real one in its
/// memory, so the shell reads it once a frame with [`read_window_rects`].
pub type WindowRects = std::collections::BTreeMap<usize, (f32, f32, f32, f32)>;

/// Reads the rectangle of every floating window of `dock` from egui's memory (a window not yet
/// drawn has none and is missing from the map).
#[must_use]
pub fn read_window_rects(ctx: &egui::Context, dock: &DockState<Panel>) -> WindowRects {
    dock.iter_surfaces_indexed()
        .filter(|(_, surface)| matches!(surface, egui_dock::Surface::Window(..)))
        .filter_map(|(index, _)| {
            // The id egui_dock gives the window's area.
            let rect = ctx.memory(|m| m.area_rect(egui::Id::new(format!("window {index:?}"))))?;
            (rect.min.x.is_finite() && rect.width().is_finite())
                .then_some((index.0, (rect.min.x, rect.min.y, rect.width(), rect.height())))
        })
        .collect()
}

/// Whether every floating window of `dock` has a rectangle in `rects` (has been drawn), so
/// [`clamp_floating_windows`] has seen them all. True when there are none.
#[must_use]
pub fn floating_windows_shown(dock: &DockState<Panel>, rects: &WindowRects) -> bool {
    dock.iter_surfaces_indexed()
        .filter(|(_, surface)| matches!(surface, egui_dock::Surface::Window(..)))
        .all(|(index, _)| rects.contains_key(&index.0))
}

/// Clamps every drawn floating window of `dock` onto a screen of `screen` points (see
/// [`clamp_window`]) by queueing a new position and size for the next frame; returns how many
/// moved or resized. A window not yet drawn is left alone.
pub fn clamp_floating_windows(dock: &mut DockState<Panel>, rects: &WindowRects, screen: [f32; 2]) -> usize {
    let mut moved = 0;
    for (&index, &(x, y, w, h)) in rects {
        let (pos, size) = clamp_window([x, y], [w, h], screen);
        if pos == [x, y] && size == [w, h] {
            continue;
        }
        if let Some(state) = dock.get_window_state_mut(egui_dock::SurfaceIndex(index)) {
            state
                .set_position(egui::pos2(pos[0], pos[1]))
                .set_size(egui::vec2(size[0], size[1]));
            moved += 1;
        }
    }
    moved
}

/// A fingerprint of the window rectangles, in whole points (a pixel of jitter does not change it).
#[must_use]
pub fn rects_fingerprint(rects: &WindowRects) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (index, (x, y, w, h)) in rects {
        (
            *index,
            x.round() as i32,
            y.round() as i32,
            w.round() as i32,
            h.round() as i32,
        )
            .hash(&mut hasher);
    }
    hasher.finish()
}

/// Why a layout was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    /// Not a layout file.
    Parse(String),
    /// Written by a newer editor.
    Newer {
        /// The file's version.
        found: u32,
    },
    /// Too deep, too many tabs, or no panel left.
    Invalid(String),
    /// Reading or writing failed.
    Io(String),
}

impl LayoutError {
    /// The agent error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "layout_parse",
            Self::Newer { .. } => "layout_newer",
            Self::Invalid(_) => "layout_invalid",
            Self::Io(_) => "layout_io",
        }
    }
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(m) | Self::Invalid(m) | Self::Io(m) => f.write_str(m),
            Self::Newer { found } => write!(
                f,
                "written by a newer editor (format_version {found}, this editor reads up to {EDITOR_LAYOUT_FORMAT_VERSION}); left untouched"
            ),
        }
    }
}

impl std::error::Error for LayoutError {}

/// The `from` versions of the registered `editor_layout.ron` migration steps (1 -> 2 added
/// `floating`, 2 -> 3 added panel instances).
#[must_use]
pub fn layout_migration_sources() -> Vec<u32> {
    // 1 -> 2: `floating` is new and defaults to none, so a version 1 file reads as it is.
    // 2 -> 3: `instances` is new and defaults to none, and every bare id is instance 0.
    vec![1, 2]
}

/// The serde shape of `editor_layout.ron` and the named layouts (ADR-7902: the `editor-layout`
/// format).
#[must_use]
pub fn layout_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<LayoutDoc>(),
        regia_core::schema::serde_field_names::<FloatingWindow>(),
        regia_core::schema::serde_field_names::<PanelInstanceState>(),
        regia_core::schema::serde_variant_names::<LayoutNode>(),
        regia_core::schema::serde_variant_shapes::<LayoutNode>(),
        regia_core::schema::serde_variant_names::<SplitAxis>(),
    ])
}

impl LayoutNode {
    fn depth(&self) -> usize {
        match self {
            Self::Tabs { .. } => 0,
            Self::Split { first, second, .. } => 1 + first.depth().max(second.depth()),
        }
    }

    fn tab_count(&self) -> usize {
        match self {
            Self::Tabs { tabs, .. } => tabs.len(),
            Self::Split { first, second, .. } => first.tab_count() + second.tab_count(),
        }
    }

    /// Every panel id in the tree, depth first.
    fn ids<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Self::Tabs { tabs, .. } => out.extend(tabs.iter().map(String::as_str)),
            Self::Split { first, second, .. } => {
                first.ids(out);
                second.ids(out);
            }
        }
    }

    /// Keeps only resolvable, first-seen panels; `None` when nothing is left.
    fn resolve(&self, seen: &mut Vec<Panel>, dropped: &mut Vec<String>) -> Option<Resolved> {
        match self {
            Self::Tabs { tabs, active } => {
                let shown = tabs.get(*active).and_then(|id| Panel::from_id(id));
                let mut panels = Vec::new();
                for id in tabs {
                    match Panel::from_id(id) {
                        Some(panel) if !seen.contains(&panel) => {
                            seen.push(panel);
                            panels.push(panel);
                        }
                        _ => dropped.push(id.clone()),
                    }
                }
                if panels.is_empty() {
                    return None;
                }
                let active = shown
                    .and_then(|p| panels.iter().position(|q| *q == p))
                    .unwrap_or(0);
                Some(Resolved::Tabs(panels, active))
            }
            Self::Split {
                axis,
                fraction,
                first,
                second,
            } => {
                let a = first.resolve(seen, dropped);
                let b = second.resolve(seen, dropped);
                match (a, b) {
                    (Some(a), Some(b)) => {
                        let fraction = if fraction.is_finite() {
                            fraction.clamp(0.05, 0.95)
                        } else {
                            0.5
                        };
                        Some(Resolved::Split(*axis, fraction, Box::new(a), Box::new(b)))
                    }
                    (Some(only), None) | (None, Some(only)) => Some(only),
                    (None, None) => None,
                }
            }
        }
    }
}

/// A normalized node: real panels only.
enum Resolved {
    Tabs(Vec<Panel>, usize),
    Split(SplitAxis, f32, Box<Resolved>, Box<Resolved>),
}

impl Resolved {
    fn first_tabs(&self) -> &[Panel] {
        match self {
            Self::Tabs(tabs, _) => tabs,
            Self::Split(_, _, first, _) => first.first_tabs(),
        }
    }

    /// The tab group holding `panel`, as a mutable list.
    fn group_with(&mut self, panel: Panel) -> Option<&mut Vec<Panel>> {
        match self {
            Self::Tabs(tabs, _) => tabs.contains(&panel).then_some(tabs),
            Self::Split(_, _, first, second) => match first.group_with(panel) {
                Some(group) => Some(group),
                None => second.group_with(panel),
            },
        }
    }

    /// Builds this node into `dock` at `index`, whose leaf already holds [`Self::first_tabs`].
    fn place(&self, dock: &mut DockState<Panel>, index: NodeIndex) {
        match self {
            Self::Tabs(tabs, active) => {
                if let Node::Leaf(leaf) = &mut dock.main_surface_mut()[index] {
                    leaf.active = TabIndex((*active).min(tabs.len().saturating_sub(1)));
                }
            }
            Self::Split(axis, fraction, first, second) => {
                let tree = dock.main_surface_mut();
                let new = second.first_tabs().to_vec();
                let [a, b] = match axis {
                    SplitAxis::Horizontal => tree.split_right(index, *fraction, new),
                    SplitAxis::Vertical => tree.split_below(index, *fraction, new),
                };
                first.place(dock, a);
                second.place(dock, b);
            }
        }
    }
}

/// What restoring a layout dropped or added.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// Ids that named no built-in panel (or repeated one).
    pub dropped: Vec<String>,
    /// Panels added because the saving editor did not have them.
    pub added: Vec<&'static str>,
}

impl LayoutDoc {
    /// The layout of `dock`: the main surface and the floating windows, without their rectangles
    /// (use [`Self::from_dock_with`] when they are known).
    #[must_use]
    pub fn from_dock(dock: &DockState<Panel>) -> Self {
        Self::from_dock_with(dock, &WindowRects::new())
    }

    /// The layout of `dock`, the floating windows with the rectangles in `rects`.
    #[must_use]
    pub fn from_dock_with(dock: &DockState<Panel>, rects: &WindowRects) -> Self {
        let tree = dock.main_surface();
        let root = node_at(tree, NodeIndex::root(), 0).unwrap_or(LayoutNode::Tabs {
            tabs: vec![Panel::Viewport.id().to_owned()],
            active: 0,
        });
        Self {
            format_version: EDITOR_LAYOUT_FORMAT_VERSION,
            root,
            known_panels: BUILT_IN_PANELS.iter().map(|p| p.id().to_owned()).collect(),
            floating: floating_of(dock, rects),
            instances: std::collections::BTreeMap::new(),
        }
    }

    /// Parses layout text of any supported version and checks its size.
    ///
    /// # Errors
    ///
    /// [`LayoutError::Parse`], [`LayoutError::Newer`] or [`LayoutError::Invalid`].
    pub fn from_text(text: &str) -> Result<Self, LayoutError> {
        let mut doc = crate::editor_prefs::read_versioned(text, EDITOR_LAYOUT_FORMAT_VERSION, |d: &Self| {
            d.format_version
        })
        .map_err(|e| match e {
            crate::editor_prefs::VersionedError::Parse(m) => LayoutError::Parse(m),
            crate::editor_prefs::VersionedError::Newer(found) => LayoutError::Newer { found },
        })?;
        doc.format_version = EDITOR_LAYOUT_FORMAT_VERSION;
        doc.check()?;
        Ok(doc)
    }

    /// Checks the depth and size limits.
    ///
    /// # Errors
    ///
    /// [`LayoutError::Invalid`].
    pub fn check(&self) -> Result<(), LayoutError> {
        if self.root.depth() > MAX_DEPTH {
            return Err(LayoutError::Invalid(format!(
                "the layout nests deeper than {MAX_DEPTH} splits"
            )));
        }
        if self.floating.len() > MAX_FLOATING {
            return Err(LayoutError::Invalid(format!(
                "the layout names more than {MAX_FLOATING} floating windows"
            )));
        }
        let floating_tabs: usize = self.floating.iter().map(|w| w.tabs.len()).sum();
        if self.root.tab_count() + floating_tabs > MAX_TABS || self.known_panels.len() > MAX_TABS {
            return Err(LayoutError::Invalid(format!(
                "the layout names more than {MAX_TABS} tabs"
            )));
        }
        Ok(())
    }

    /// The file text (current version).
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut doc = self.clone();
        doc.format_version = EDITOR_LAYOUT_FORMAT_VERSION;
        ron::ser::to_string_pretty(&doc, regia_core::ron_pretty().struct_names(false)).unwrap_or_default()
    }

    /// Every panel id the layout names, depth first.
    #[must_use]
    pub fn panel_ids(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.root.ids(&mut out);
        out
    }

    /// Builds the dock, normalizing as the module docs describe.
    ///
    /// # Errors
    ///
    /// [`LayoutError::Invalid`] when the layout is too large or names no panel at all.
    pub fn to_dock(&self) -> Result<(DockState<Panel>, RestoreReport), LayoutError> {
        self.check()?;
        let mut report = RestoreReport::default();
        let mut seen = Vec::new();
        let mut resolved = self
            .root
            .resolve(&mut seen, &mut report.dropped)
            .ok_or_else(|| LayoutError::Invalid("the layout names no editor panel".to_owned()))?;
        // Panels of the default layout the saving editor did not have go to the bottom group (where
        // the default layout has them), or the first group.
        for panel in std::iter::once(Panel::Viewport)
            .chain([Panel::Outliner, Panel::Inspector])
            .chain(DEFAULT_BOTTOM.iter().copied())
        {
            if seen.contains(&panel) || self.known_panels.iter().any(|k| k == panel.id()) {
                continue;
            }
            // A floating window claims this panel: docking it here would mark it seen and the
            // floating loop below would then drop the window (a restore must not lose it).
            if self
                .floating
                .iter()
                .any(|window| window.tabs.iter().any(|id| id == panel.id()))
            {
                continue;
            }
            let target = if seen.contains(&Panel::Content) {
                Panel::Content
            } else {
                resolved.first_tabs()[0]
            };
            if let Some(group) = resolved.group_with(target) {
                group.push(panel);
            }
            seen.push(panel);
            report.added.push(panel.id());
        }
        let mut dock = DockState::new(resolved.first_tabs().to_vec());
        resolved.place(&mut dock, NodeIndex::root());
        // Floating windows: the same normalization (unknown, extension and repeated panels drop; a
        // window left with none is dropped), so a panel is never in two places.
        for window in &self.floating {
            let mut panels = Vec::new();
            for id in &window.tabs {
                match Panel::from_id(id) {
                    Some(panel) if !seen.contains(&panel) => {
                        seen.push(panel);
                        panels.push(panel);
                    }
                    _ => report.dropped.push(id.clone()),
                }
            }
            if panels.is_empty() {
                continue;
            }
            let shown = window
                .tabs
                .get(window.active)
                .and_then(|id| Panel::from_id(id))
                .and_then(|p| panels.iter().position(|q| *q == p))
                .unwrap_or(0);
            let first = panels.first().copied();
            let index = dock.add_window(panels);
            if let Some(egui_dock::Surface::Window(tree, _)) = dock.get_surface_mut(index)
                && let Node::Leaf(leaf) = &mut tree[NodeIndex::root()]
            {
                leaf.active = TabIndex(shown);
            }
            if let Some((x, y, w, h)) = window.rect
                && let Some(state) = dock.get_window_state_mut(index)
            {
                let (pos, size) = clamp_window([x, y], [w, h], [f32::MAX, f32::MAX]);
                // An OS window (ADR-7821) opens where it was: egui_dock keeps the queued position
                // private, so the window's first panel keys it for `take_restored_rect`.
                if let Some(first) = first {
                    remember_restored_rect(first, (pos[0], pos[1], size[0], size[1]));
                }
                state
                    .set_position(egui::pos2(pos[0], pos[1]))
                    .set_size(egui::vec2(size[0], size[1]));
            }
        }
        Ok((dock, report))
    }
}

/// The saved floating windows of `dock`: each window's panels flattened into one tab group (an
/// extension panel is not saved), with its rectangle when it has been shown.
fn floating_of(dock: &DockState<Panel>, rects: &WindowRects) -> Vec<FloatingWindow> {
    dock.iter_surfaces_indexed()
        .filter_map(|(index, surface)| {
            let egui_dock::Surface::Window(tree, _) = surface else {
                return None;
            };
            let tabs: Vec<String> = tree
                .iter()
                .flat_map(|node| match node {
                    Node::Leaf(leaf) => leaf.tabs.clone(),
                    _ => Vec::new(),
                })
                .filter(|p| {
                    !matches!(
                        p,
                        Panel::ExtensionPanel(..) | Panel::GameView(_) | Panel::UtilityPanel(..)
                    )
                })
                .map(|p| p.id().to_owned())
                .collect();
            if tabs.is_empty() {
                return None;
            }
            let active = match &tree[NodeIndex::root()] {
                Node::Leaf(leaf) => leaf
                    .tabs
                    .get(leaf.active.0)
                    .and_then(|p| tabs.iter().position(|id| id == p.id()))
                    .unwrap_or(0),
                _ => 0,
            };
            let rect = rects.get(&index.0).copied().filter(|r| r.2 > 0.0 && r.3 > 0.0);
            Some(FloatingWindow { tabs, active, rect })
        })
        .collect()
}

/// The saved node at `index` (`None` for an empty node).
fn node_at(tree: &egui_dock::Tree<Panel>, index: NodeIndex, depth: usize) -> Option<LayoutNode> {
    if index.0 >= tree.len() || depth > MAX_DEPTH {
        return None;
    }
    match &tree[index] {
        Node::Empty => None,
        Node::Leaf(leaf) => {
            let tabs: Vec<String> = leaf
                .tabs
                .iter()
                .filter(|p| {
                    !matches!(
                        p,
                        Panel::ExtensionPanel(..) | Panel::GameView(_) | Panel::UtilityPanel(..)
                    )
                })
                .map(|p| p.id().to_owned())
                .collect();
            let shown = leaf.tabs.get(leaf.active.0).copied();
            let active = shown
                .and_then(|p| tabs.iter().position(|id| id == p.id()))
                .unwrap_or(0);
            (!tabs.is_empty()).then_some(LayoutNode::Tabs { tabs, active })
        }
        Node::Horizontal(split) | Node::Vertical(split) => {
            let axis = if matches!(tree[index], Node::Horizontal(_)) {
                SplitAxis::Horizontal
            } else {
                SplitAxis::Vertical
            };
            let first = node_at(tree, index.left(), depth + 1);
            let second = node_at(tree, index.right(), depth + 1);
            match (first, second) {
                (Some(first), Some(second)) => Some(LayoutNode::Split {
                    axis,
                    fraction: split.fraction,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            }
        }
    }
}

/// A cheap fingerprint of the dock (main surface node kinds, fractions, tabs, shown tabs, and the
/// floating windows' tabs; their rectangles are [`rects_fingerprint`]) that
/// allocates nothing, so the shell can tell each frame whether the layout changed.
#[must_use]
pub fn dock_fingerprint(dock: &DockState<Panel>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (i, node) in dock.main_surface().iter().enumerate() {
        match node {
            Node::Empty => continue,
            Node::Leaf(leaf) => {
                (i, 0u8, leaf.active.0).hash(&mut hasher);
                for tab in &leaf.tabs {
                    tab.hash(&mut hasher);
                }
            }
            Node::Horizontal(split) => (i, 1u8, split.fraction.to_bits()).hash(&mut hasher),
            Node::Vertical(split) => (i, 2u8, split.fraction.to_bits()).hash(&mut hasher),
        }
    }
    for surface in dock.iter_surfaces() {
        if let egui_dock::Surface::Window(tree, _) = surface {
            for node in tree.iter() {
                if let Node::Leaf(leaf) = node {
                    (3u8, leaf.active.0).hash(&mut hasher);
                    for tab in &leaf.tabs {
                        tab.hash(&mut hasher);
                    }
                }
            }
        }
    }
    hasher.finish()
}

/// Where the shown layout came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutSource {
    /// The built-in layout.
    #[default]
    Default,
    /// The project's `.regia/editor_layout.ron`.
    Project,
    /// The user's default layout.
    User,
    /// Applied by an agent or a Reset this session.
    Session,
}

impl LayoutSource {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Project => "project",
            Self::User => "user",
            Self::Session => "session",
        }
    }
}

/// The layout the shell shows, published when it changes (for the agent and the menus).
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct LayoutSnapshot {
    /// The current layout.
    pub doc: LayoutDoc,
    /// [`dock_fingerprint`] of it.
    pub fingerprint: u64,
    /// Where it came from.
    pub source: LayoutSource,
    /// What the last restore dropped or added.
    pub report: RestoreReport,
}

/// Read-only observations of the last UI pass, never persisted in a layout file (ADR-7955).
#[derive(Resource, Debug, Default)]
pub(crate) struct LayoutObservation {
    /// Visible panel bodies in logical points, `[x, y, width, height]`, excluding dock chrome.
    pub panel_regions: std::collections::BTreeMap<String, [f32; 4]>,
    /// Scale from logical points to screenshot pixels.
    pub pixels_per_point: f32,
    /// Opaque egui widget id, or no keyboard focus.
    pub focused_widget: Option<String>,
}

impl LayoutObservation {
    /// Start a fresh pass so hidden or closed panels cannot retain stale rectangles.
    pub fn begin(ctx: &egui::Context) -> Self {
        Self {
            pixels_per_point: ctx.pixels_per_point(),
            ..Self::default()
        }
    }

    /// Record only the body that this panel's draw function can paint.
    pub fn record_panel(&mut self, id: &str, ui: &egui::Ui) {
        let rect = ui.max_rect().intersect(ui.clip_rect());
        self.panel_regions.insert(
            id.to_owned(),
            [rect.min.x, rect.min.y, rect.width(), rect.height()],
        );
    }

    /// Observe keyboard focus after the panels and modal windows have drawn.
    pub fn finish(&mut self, ctx: &egui::Context) {
        self.focused_widget = ctx
            .memory(|m| m.focused())
            .map(|id| format!("{:016x}", id.value()));
    }
}

/// A layout action for the shell to perform on its next frame.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutRequest {
    /// Back to the built-in layout.
    Reset,
    /// Show this layout.
    Apply(LayoutDoc),
    /// Re-read the project's (or the user's) saved layout.
    Reload,
    /// Bring a panel's tab forward (adding it when closed).
    Open(Panel),
    /// Save the shown layout into the project.
    SaveProject,
    /// Save the shown layout as the user's default.
    SaveUser,
    /// The crash-recovery window's Restore: the first pending autosave becomes the open level
    /// (`editor.command { run: "recovery.restore" }`, ADR-7307).
    RestoreRecovery,
    /// The crash-recovery window's Discard autosave (`recovery.discard`, ADR-7307).
    DiscardRecovery,
    /// The Material panel's Undo (`material.undo`, ADR-7307).
    MaterialUndo,
    /// The Material panel's Redo (`material.redo`, ADR-7307).
    MaterialRedo,
    /// The Material panel's Reload from disk (`material.reload`, ADR-7307).
    MaterialReload,
    /// Open a new panel instance (`Panel::Instance`) next to its panel (Window ▸ New Outliner,
    /// `editor.panel_new_instance`; ADR-8105 (a)).
    NewInstance(Panel),
}

/// Pending [`LayoutRequest`]s (a resource the shell drains each frame).
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct LayoutRequests {
    /// In arrival order.
    pub pending: Vec<LayoutRequest>,
}

impl LayoutRequests {
    /// Queues `request` in `world`.
    pub fn push(world: &mut World, request: LayoutRequest) {
        world
            .get_resource_or_insert_with(Self::default)
            .pending
            .push(request);
    }
}

/// The project's layout file.
#[must_use]
pub fn project_layout_path(root: &Path) -> PathBuf {
    root.join(PROJECT_LAYOUT_FILE)
}

/// Reads a layout file; `Ok(None)` when it does not exist.
///
/// # Errors
///
/// The file cannot be read, does not parse, is too large, or is from a newer editor.
pub fn read_layout_file(path: &Path) -> Result<Option<LayoutDoc>, LayoutError> {
    match std::fs::read_to_string(path) {
        Ok(text) => LayoutDoc::from_text(&text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(LayoutError::Io(format!("cannot read {}: {e}", path.display()))),
    }
}

/// Writes `doc` to `path` atomically. A file written by a newer editor is left untouched.
///
/// # Errors
///
/// [`LayoutError::Newer`] for a newer file in the way, [`LayoutError::Io`] when writing failed.
pub fn write_layout_file(path: &Path, doc: &LayoutDoc) -> Result<(), LayoutError> {
    if let Ok(text) = std::fs::read_to_string(path)
        && let Err(LayoutError::Newer { found }) = LayoutDoc::from_text(&text)
    {
        return Err(LayoutError::Newer { found });
    }
    crate::level_io::write_atomic(path, doc.to_text().as_bytes()).map_err(LayoutError::Io)
}

/// The layout a project opens with: its own file, else the user's default (`user_file`), else
/// `None` (the built-in layout). Unreadable files are skipped with a warning.
#[must_use]
pub fn saved_layout_for(
    project: Option<&Path>,
    user_file: Option<&Path>,
) -> Option<(LayoutDoc, LayoutSource)> {
    let candidates = [
        project.map(|root| (project_layout_path(root), LayoutSource::Project)),
        user_file.map(|file| (file.to_path_buf(), LayoutSource::User)),
    ];
    for (path, source) in candidates.into_iter().flatten() {
        match read_layout_file(&path) {
            Ok(Some(doc)) => return Some((doc, source)),
            Ok(None) => {}
            Err(err) => tracing::warn!(target: "regia_editor", "{}: {err}", path.display()),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn panel_ids_are_unique_and_round_trip() {
        let mut seen = std::collections::HashSet::new();
        for panel in BUILT_IN_PANELS {
            assert!(seen.insert(panel.id()), "duplicate {}", panel.id());
            assert_eq!(Panel::from_id(panel.id()), Some(*panel));
        }
        assert_eq!(Panel::from_id("ext"), None);
    }

    #[test]
    fn the_default_layout_round_trips_through_the_doc() {
        let dock = default_dock();
        let doc = LayoutDoc::from_dock(&dock);
        let (back, report) = doc.to_dock().unwrap();
        assert_eq!(report, RestoreReport::default());
        assert_eq!(LayoutDoc::from_dock(&back), doc);
        assert_eq!(dock_fingerprint(&back), dock_fingerprint(&dock));
        let text = doc.to_text();
        assert_eq!(LayoutDoc::from_text(&text).unwrap(), doc);
    }

    // ----- floating windows (ADR-2771) -----

    struct Tabs;
    impl egui_dock::TabViewer for Tabs {
        type Tab = Panel;
        fn title(&mut self, tab: &mut Panel) -> egui::WidgetText {
            tab.id().into()
        }
        fn id(&mut self, tab: &mut Panel) -> egui::Id {
            egui::Id::new(tab.id())
        }
        fn ui(&mut self, _ui: &mut egui::Ui, _tab: &mut Panel) {}
    }

    /// Draws `dock` on a screen `w` x `h` (two frames: egui windows size themselves on the first)
    /// and reads the floating windows' rectangles.
    fn draw(ctx: &egui::Context, dock: &mut DockState<Panel>, w: f32, h: f32) -> WindowRects {
        for _ in 0..2 {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w, h))),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| {
                egui_dock::DockArea::new(dock).show_inside(ui, &mut Tabs);
            });
            out.textures_delta.clear();
        }
        read_window_rects(ctx, dock)
    }

    fn dock_with_windows(ctx: &egui::Context) -> (DockState<Panel>, WindowRects) {
        let mut dock = DockState::new(vec![Panel::Viewport, Panel::Outliner]);
        let a = dock.add_window(vec![Panel::Problems, Panel::Tables]);
        let b = dock.add_window(vec![Panel::Cameras]);
        dock.get_window_state_mut(a)
            .unwrap()
            .set_position(egui::pos2(100.0, 80.0))
            .set_size(egui::vec2(400.0, 300.0));
        dock.get_window_state_mut(b)
            .unwrap()
            .set_position(egui::pos2(600.0, 200.0))
            .set_size(egui::vec2(300.0, 200.0));
        let rects = draw(ctx, &mut dock, 1280.0, 800.0);
        (dock, rects)
    }

    fn close(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
        [a.0 - b.0, a.1 - b.1, a.2 - b.2, a.3 - b.3]
            .iter()
            .all(|d| d.abs() < 1.5)
    }

    #[test]
    fn clamping_keeps_a_window_on_the_screen() {
        let screen = [1280.0, 800.0];
        // Already on screen: untouched.
        assert_eq!(
            clamp_window([100.0, 80.0], [400.0, 300.0], screen),
            ([100.0, 80.0], [400.0, 300.0])
        );
        // Off the right and bottom edges: pulled back so the whole window shows.
        assert_eq!(
            clamp_window([1200.0, 700.0], [400.0, 300.0], screen),
            ([880.0, 500.0], [400.0, 300.0])
        );
        // Left of and above the origin: moved to it.
        assert_eq!(
            clamp_window([-500.0, -40.0], [400.0, 300.0], screen),
            ([0.0, 0.0], [400.0, 300.0])
        );
        // Bigger than a (smaller) monitor: shrunk to it, at the origin.
        assert_eq!(
            clamp_window([300.0, 300.0], [3000.0, 2000.0], [1024.0, 600.0]),
            ([0.0, 0.0], [1024.0, 600.0])
        );
        // Too small: raised to the minimum.
        assert_eq!(clamp_window([10.0, 10.0], [1.0, 1.0], screen).1, MIN_WINDOW_SIZE);
        // NaN and infinity never leak through.
        let (pos, size) = clamp_window([f32::NAN, f32::INFINITY], [f32::NAN, f32::NEG_INFINITY], screen);
        assert!(pos.iter().chain(&size).all(|v| v.is_finite()));
        assert_eq!(size, DEFAULT_WINDOW_SIZE);
        // A screen smaller than the minimum window still gives a finite, non-negative answer.
        let (pos, size) = clamp_window([5.0, 5.0], [500.0, 500.0], [10.0, 10.0]);
        assert_eq!((pos, size), ([0.0, 0.0], MIN_WINDOW_SIZE));
        // Always inside the screen when the screen can hold the minimum.
        for (px, py, w, h) in [
            (-9e3, 9e3, 50.0, 70.0),
            (1e9, 1e9, 1e9, 1e9),
            (0.0, 0.0, 0.0, 0.0),
        ] {
            let (p, sz) = clamp_window([px, py], [w, h], screen);
            assert!(
                p[0] >= 0.0 && p[1] >= 0.0 && p[0] + sz[0] <= screen[0] && p[1] + sz[1] <= screen[1],
                "{p:?} {sz:?}"
            );
        }
    }

    #[test]
    fn floating_windows_are_saved_and_restored() {
        let ctx = egui::Context::default();
        let (dock, rects) = dock_with_windows(&ctx);
        assert_eq!(rects.len(), 2, "both windows were drawn: {rects:?}");
        let doc = LayoutDoc::from_dock_with(&dock, &rects);
        // The current version (2 when written; 3 since ADR-8105 (a)).
        assert_eq!(doc.format_version, EDITOR_LAYOUT_FORMAT_VERSION);
        assert_eq!(doc.floating.len(), 2, "{:?}", doc.floating);
        assert_eq!(doc.floating[0].tabs, ["problems", "tables"]);
        assert_eq!(doc.floating[1].tabs, ["cameras"]);
        let (x, y, w, h) = doc.floating[0].rect.expect("drawn, so it has a rectangle");
        assert!(
            (x - 100.0).abs() < 1.5 && (y - 80.0).abs() < 1.5,
            "position as set: {x} {y}"
        );
        assert_eq!((w, h), (400.0, 300.0), "the saved size is the whole window");
        // Without rectangles the windows are still saved, just not placed.
        assert!(
            LayoutDoc::from_dock(&dock)
                .floating
                .iter()
                .all(|f| f.rect.is_none())
        );
        // Through the file text and back, then onto a dock and drawn: the same windows in the same place.
        let back = LayoutDoc::from_text(&doc.to_text()).unwrap();
        assert_eq!(back, doc);
        let (mut restored, report) = back.to_dock().unwrap();
        assert!(report.dropped.is_empty(), "{report:?}");
        let fresh = egui::Context::default();
        let again = draw(&fresh, &mut restored, 1280.0, 800.0);
        let second = LayoutDoc::from_dock_with(&restored, &again);
        assert_eq!(second.floating.len(), 2);
        for (a, b) in doc.floating.iter().zip(&second.floating) {
            assert_eq!((&a.tabs, a.active), (&b.tabs, b.active));
            assert!(
                close(a.rect.unwrap(), b.rect.unwrap()),
                "no drift: {a:?} vs {b:?}"
            );
        }
        // Saving again and restoring again does not grow the windows.
        let (mut third, _) = second.to_dock().unwrap();
        let third_rects = draw(&egui::Context::default(), &mut third, 1280.0, 800.0);
        let third_doc = LayoutDoc::from_dock_with(&third, &third_rects);
        assert!(close(
            third_doc.floating[0].rect.unwrap(),
            doc.floating[0].rect.unwrap()
        ));
        // The fingerprints see tabs and moved windows, not jitter.
        assert_eq!(dock_fingerprint(&dock), dock_fingerprint(&restored));
        let mut jitter = rects.clone();
        for r in jitter.values_mut() {
            r.0 += 0.2;
        }
        assert_eq!(rects_fingerprint(&rects), rects_fingerprint(&jitter));
        let mut moved = rects.clone();
        if let Some(r) = moved.values_mut().next() {
            r.0 += 40.0;
        }
        assert_ne!(rects_fingerprint(&rects), rects_fingerprint(&moved));
        let plain = DockState::new(vec![Panel::Viewport, Panel::Outliner]);
        assert_ne!(dock_fingerprint(&plain), dock_fingerprint(&dock));
    }

    #[test]
    fn a_floating_panel_is_not_docked_when_the_saving_editor_lacked_other_panels() {
        // A hand-written (older or other-editor) layout: three panels were known, Problems was
        // kept in a floating window. Restore must keep Problems floating; the added-panels loop
        // used to claim it first and the floating loop then dropped the window as a repeat.
        let doc = LayoutDoc {
            format_version: EDITOR_LAYOUT_FORMAT_VERSION,
            root: LayoutNode::Tabs {
                tabs: ["viewport", "outliner", "content"].map(str::to_owned).to_vec(),
                active: 0,
            },
            known_panels: ["viewport", "outliner", "content"].map(str::to_owned).to_vec(),
            floating: vec![FloatingWindow {
                tabs: vec!["problems".to_owned()],
                active: 0,
                rect: Some((40.0, 60.0, 480.0, 320.0)),
            }],
            instances: Default::default(),
        };
        let (dock, report) = doc.to_dock().unwrap();
        assert!(report.dropped.is_empty(), "{report:?}");
        assert!(
            !report.added.contains(&"problems"),
            "Problems keeps its floating window instead of being docked: {report:?}"
        );
        let mut windows = dock
            .iter_surfaces()
            .filter_map(|surface| match surface {
                egui_dock::Surface::Window(tree, _) => Some(tree.tabs().copied().collect::<Vec<_>>()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(windows.len(), 1, "the floating window survives: {windows:?}");
        assert_eq!(windows.pop().expect("one window"), [Panel::Problems]);
        let main: Vec<Panel> = dock.main_surface().tabs().copied().collect();
        assert!(
            !main.contains(&Panel::Problems),
            "Problems is not also docked: {main:?}"
        );
    }

    #[test]
    fn clamp_floating_windows_queues_a_move_only_for_a_window_off_the_screen() {
        let ctx = egui::Context::default();
        let (mut dock, rects) = dock_with_windows(&ctx);
        assert_eq!(
            clamp_floating_windows(&mut dock, &rects, [1280.0, 800.0]),
            0,
            "all on screen"
        );
        // The same windows on a monitor of 500 x 300: both stick out.
        assert_eq!(clamp_floating_windows(&mut dock, &rects, [500.0, 300.0]), 2);
        // A window with no rectangle yet is left alone.
        assert_eq!(
            clamp_floating_windows(&mut dock, &WindowRects::new(), [500.0, 300.0]),
            0
        );
        assert!(floating_windows_shown(&dock, &rects));
        assert!(!floating_windows_shown(&dock, &WindowRects::new()));
        assert!(floating_windows_shown(
            &DockState::new(vec![Panel::Viewport]),
            &WindowRects::new()
        ));
    }

    #[test]
    fn a_window_for_a_smaller_monitor_ends_up_on_the_screen() {
        let ctx = egui::Context::default();
        let (dock, rects) = dock_with_windows(&ctx);
        let doc = LayoutDoc::from_dock_with(&dock, &rects);
        let (mut restored, _) = doc.to_dock().unwrap();
        let small = egui::Context::default();
        let first = draw(&small, &mut restored, 640.0, 360.0);
        clamp_floating_windows(&mut restored, &first, [640.0, 360.0]);
        let after = draw(&small, &mut restored, 640.0, 360.0);
        assert!(floating_windows_shown(&restored, &after));
        for (x, y, w, h) in after.values() {
            assert!(
                *x >= -0.5 && *y >= -0.5 && x + w <= 640.5 && y + h <= 360.5,
                "({x}, {y}, {w}, {h})"
            );
        }
    }

    #[test]
    fn restoring_floating_windows_normalizes_like_the_main_surface() {
        let mut doc = LayoutDoc::from_dock(&DockState::new(vec![Panel::Viewport]));
        doc.floating = vec![
            // A repeat of the main surface's panel, an unknown id and an extension id drop out.
            FloatingWindow {
                tabs: vec!["viewport".into(), "nope".into(), "problems".into()],
                active: 2,
                rect: None,
            },
            // Nothing resolvable: the whole window is dropped.
            FloatingWindow {
                tabs: vec!["ext".into(), "nope".into()],
                active: 0,
                rect: Some((0.0, 0.0, 9.0, 9.0)),
            },
            // The same panel in a second window is dropped (a panel is in one place).
            FloatingWindow {
                tabs: vec!["problems".into(), "layers".into()],
                active: 9,
                rect: None,
            },
        ];
        let (dock, report) = doc.to_dock().unwrap();
        assert_eq!(report.dropped, ["viewport", "nope", "ext", "nope", "problems"]);
        let saved = LayoutDoc::from_dock(&dock).floating;
        assert_eq!(saved.len(), 2, "{saved:?}");
        assert_eq!(
            (saved[0].tabs.as_slice(), saved[0].active),
            (["problems".to_owned()].as_slice(), 0)
        );
        assert_eq!(saved[1].tabs, ["layers"]);
        // Too many windows is refused, not built.
        let mut many = doc.clone();
        many.floating = (0..=MAX_FLOATING)
            .map(|_| FloatingWindow {
                tabs: vec!["layers".into()],
                active: 0,
                rect: None,
            })
            .collect();
        assert_eq!(many.check().unwrap_err().code(), "layout_invalid");
    }

    #[test]
    fn a_version_1_file_reads_as_version_2_with_no_floating_windows() {
        // 1 -> 2 (floating) is still a registered step; 2 -> 3 (instances) follows it (ADR-8105).
        assert!(layout_migration_sources().contains(&1));
        let v1 = "(format_version: 1, root: Tabs(tabs: [\"viewport\", \"outliner\"], active: 1))";
        let doc = LayoutDoc::from_text(v1).unwrap();
        assert_eq!(doc.format_version, EDITOR_LAYOUT_FORMAT_VERSION);
        assert!(doc.floating.is_empty());
        let (dock, report) = doc.to_dock().unwrap();
        assert!(report.dropped.is_empty());
        assert!(LayoutDoc::from_dock(&dock).floating.is_empty());
        // Saved again it is a current-version file (2 when written; 3 since ADR-8105 (a)) that
        // still has the same main surface.
        let text = doc.to_text();
        assert!(
            text.contains(&format!("format_version: {EDITOR_LAYOUT_FORMAT_VERSION}")),
            "{text}"
        );
        assert_eq!(LayoutDoc::from_text(&text).unwrap().root, doc.root);
        // A file from a newer editor is still refused.
        assert_eq!(
            LayoutDoc::from_text(&format!(
                "(format_version: {}, root: Tabs(tabs: []))",
                EDITOR_LAYOUT_FORMAT_VERSION + 1
            ))
            .unwrap_err()
            .code(),
            "layout_newer"
        );
    }

    // ----- panel instances (version 3, ADR-8105 (a), P-003) -----

    /// A version 2 file as the v0.5.0 editor wrote it: two docked groups and a floating window.
    const V2_LAYOUT: &str = "(
    format_version: 2,
    root: Split(axis: Horizontal, fraction: 0.2,
        first: Tabs(tabs: [\"outliner\", \"inspector\"], active: 1),
        second: Tabs(tabs: [\"viewport\", \"content\"], active: 0)),
    known_panels: [\"viewport\", \"outliner\", \"inspector\", \"content\", \"problems\"],
    floating: [(tabs: [\"problems\"], active: 0, rect: Some((40.0, 60.0, 480.0, 320.0)))],
)";

    #[test]
    fn a_v2_layout_upgrades_to_v3() {
        assert_eq!(EDITOR_LAYOUT_FORMAT_VERSION, 3);
        assert_eq!(layout_migration_sources(), [1, 2]);
        let doc = LayoutDoc::from_text(V2_LAYOUT).unwrap();
        assert_eq!(doc.format_version, 3);
        assert!(doc.instances.is_empty(), "a version 2 file has no instance state");
        let (dock, report) = doc.to_dock().unwrap();
        assert!(report.dropped.is_empty(), "{report:?}");
        // Every panel is instance 0, the plain variant.
        for panel in [Panel::Outliner, Panel::Inspector, Panel::Content] {
            assert!(dock.find_tab(&panel).is_some(), "{panel:?}");
            assert_eq!(panel.instance(), 0);
        }
        assert!(
            dock.iter_all_tabs()
                .all(|(_, tab)| !matches!(tab, Panel::Instance(..))),
            "no instance appears from a version 2 file"
        );
        // Saved again it is a version 3 file with the same tree and no `instances` field.
        let text = doc.to_text();
        assert!(text.contains("format_version: 3"), "{text}");
        assert!(!text.contains("instances"), "{text}");
        let back = LayoutDoc::from_text(&text).unwrap();
        assert_eq!(back.root, doc.root);
        assert_eq!(back.floating, doc.floating);
    }

    #[test]
    fn instance_ids_parse_only_for_the_instanced_panels_and_one_to_seven() {
        use crate::shell::{InstancedPanel, MAX_PANEL_INSTANCE};
        for kind in InstancedPanel::ALL {
            assert_eq!(kind.instance(0), Some(kind.base()));
            for n in 1..=MAX_PANEL_INSTANCE {
                let panel = kind.instance(n).unwrap();
                assert_eq!(panel.id(), format!("{}#{n}", kind.base().id()));
                assert_eq!(Panel::from_id(panel.id()), Some(panel));
                assert_eq!(panel.instance(), n);
                assert_eq!(panel.base(), kind.base());
                assert_ne!(panel.title(), kind.base().title());
            }
            assert_eq!(kind.instance(MAX_PANEL_INSTANCE + 1), None);
        }
        for id in [
            "outliner#0",
            "outliner#8",
            "outliner#01",
            "outliner#",
            "outliner#1#1",
            "outliner#+1",
            "viewport#1",
            "ext#1",
            "nothing#1",
        ] {
            assert_eq!(Panel::from_id(id), None, "{id}");
        }
    }

    #[test]
    fn instances_round_trip_and_bad_suffixes_are_dropped() {
        use crate::shell::InstancedPanel;
        let text = "(
    format_version: 3,
    root: Tabs(tabs: [\"viewport\", \"outliner\", \"outliner#1\", \"viewport#2\", \"outliner#9\", \"outliner#1\"], active: 2),
    known_panels: [\"viewport\", \"outliner\", \"inspector\", \"content\"],
    floating: [(tabs: [\"content#3\"], active: 0)],
    instances: {
        \"outliner#1\": (filter: \"kind:light\", columns: [\"type\", \"tags\"]),
        \"content#3\": (folder: \"assets/textures\", filter: \"rock\"),
    },
)";
        let doc = LayoutDoc::from_text(text).unwrap();
        let (dock, report) = doc.to_dock().unwrap();
        assert_eq!(report.dropped, ["viewport#2", "outliner#9", "outliner#1"]);
        let second = Panel::Instance(InstancedPanel::Outliner, 1);
        assert!(dock.find_tab(&second).is_some());
        assert!(
            dock.find_tab(&Panel::Instance(InstancedPanel::Content, 3))
                .is_some()
        );
        let mut back = LayoutDoc::from_dock(&dock);
        assert!(back.panel_ids().contains(&"outliner#1"));
        back.instances = doc.instances.clone();
        let again = LayoutDoc::from_text(&back.to_text()).unwrap();
        assert_eq!(again.instances["outliner#1"].filter, "kind:light");
        assert_eq!(again.instances["outliner#1"].columns, ["type", "tags"]);
        assert_eq!(again.instances["content#3"].folder, "assets/textures");
        assert_eq!(again.instances["content#3"].locked, None);
    }
}
