// SPDX-License-Identifier: MPL-2.0
//! The UI designer's preview (ADR-1811, `docs/specs/81-ui-designer.md` § 4.2): the document is
//! spawned with `regia_ui::authoring::spawn_ui` into a private headless app and laid out by the
//! runtime's own layout at the chosen window size, so every rect matches what a game gets. Sample
//! values stand in for view-model data, the project's Fluent files (or pseudo-localization) fill
//! localized spans, and nodes that cross the safe-area insets are flagged. A repeat node shows `N`
//! sample items: its template is copied into the scratch document with the samples of each item
//! (ADR-2489), so the rows lay out as they will in a game. A repeat on a `list` widget is
//! virtualized (ADR-2715): its rows are placed as the list places them, and only the rows the list
//! would materialize at the top of the scroll range count as shown ([`PreviewList`]).

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use regia_app::{App, TimeUpdateStrategy};
use regia_ecs::prelude::{Added, Entity, IntoScheduleConfigs, Query, Res, Resource, bevy_ecs};
use regia_ui::authoring::{
    BindDef, BindTargetDef, LocSpan, TextDef, TextSpanDef, UiLibrary, UiNodeDef, WidgetDef,
};
use regia_ui::text::TextContent;
use regia_ui::{BindConverter, ComputedNode, Localization, NodeName, NodeStyle, Text, UiFile, UiPlugin};

use crate::ui_designer::{DesignerError, node_kind, path_string, walk};

/// Named window sizes with their safe-area insets (left, top, right, bottom; physical px).
pub const SIZE_PRESETS: [(&str, [f32; 2], [f32; 4]); 5] = [
    ("1920x1080", [1920.0, 1080.0], [0.0; 4]),
    ("1280x720", [1280.0, 720.0], [0.0; 4]),
    // Handheld (16:10).
    ("1280x800", [1280.0, 800.0], [0.0; 4]),
    // Phone, landscape, with a camera cutout on the left and the gesture bar side on the right.
    ("2400x1080", [2400.0, 1080.0], [110.0, 0.0, 60.0, 0.0]),
    // Phone, portrait.
    ("1080x2400", [1080.0, 2400.0], [0.0, 110.0, 0.0, 60.0]),
];

/// Largest preview window edge (physical px).
pub const MAX_PREVIEW_EDGE: f32 = 8192.0;

/// Sample items a repeat node shows by default.
pub const DEFAULT_REPEAT_COUNT: usize = 3;

/// Most sample items one repeat node shows.
pub const MAX_REPEAT_COUNT: usize = 256;

/// Most nodes a preview lays out after its repeats are expanded; nested repeats multiply.
pub const MAX_PREVIEW_NODES: usize = 8192;

/// The part of [`PreviewSettings`] the editor remembers across sessions (`.regia/editor_view.ron`,
/// [`crate::view_state`]): the window size, the safe-area insets and the theme.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PreviewView {
    /// Window size (physical px).
    pub size: [f32; 2],
    /// Safe-area insets: left, top, right, bottom (physical px).
    pub safe_area: [f32; 4],
    /// The theme the themed preview draws with; `None`: the active one.
    #[serde(default)]
    pub theme: Option<String>,
}

/// What the preview shows.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewSettings {
    /// Window size (physical px).
    pub size: [f32; 2],
    /// Safe-area insets: left, top, right, bottom (physical px).
    pub safe_area: [f32; 4],
    /// Locale to resolve localized spans in (`None`: the project's first locale).
    pub locale: Option<String>,
    /// Pseudo-localize resolved text (accents and padding: finds clipped or unlocalized text).
    pub pseudo: bool,
    /// Sample values by binding source path (`"HudVm.health" -> "0.4"`). Under a repeat, a field of
    /// the item has the key `"Vm.items[].name"` (every item) or `"Vm.items[1].name"` (one item), and
    /// `"Vm.items[]#count"` sets that repeat's item count.
    pub samples: BTreeMap<String, String>,
    /// Sample items a repeat node shows when `samples` does not give a count.
    pub repeat_count: usize,
    /// The project's `assets/` directory component instances are read from (spec 111 § 6); `None`
    /// draws each instance as its error placeholder.
    pub assets_root: Option<std::path::PathBuf>,
    /// The theme the themed preview draws with (ADR-7677); `None`: the active one. View state of the
    /// open document, never written to the file.
    pub theme: Option<String>,
    /// Manual UI scale; `None` evaluates the preview rule at the selected size and DPI.
    pub scale: Option<f32>,
    /// Preview-only rule override. `None` uses the saved project scale rule when the
    /// project file has one, and the engine default otherwise (ADR-8252).
    pub scale_mode: Option<regia_ui::ScaleMode>,
    /// Simulated display DPI (96 corresponds to an OS scale factor of one).
    pub dpi: f32,
    /// Draw selected-node margin and padding bands (view state only).
    pub box_model: bool,
    /// The layout variant the designer edits (index into the document's `variants`, ADR-7858); `None`:
    /// the base, and the preview shows the variant its size picks. View state, never written.
    pub variant: Option<usize>,
}

impl Default for PreviewSettings {
    fn default() -> Self {
        Self {
            size: SIZE_PRESETS[0].1,
            safe_area: SIZE_PRESETS[0].2,
            locale: None,
            pseudo: false,
            samples: BTreeMap::new(),
            repeat_count: DEFAULT_REPEAT_COUNT,
            assets_root: None,
            theme: None,
            scale: None,
            scale_mode: None,
            dpi: 96.0,
            box_model: false,
            variant: None,
        }
    }
}

impl PreviewSettings {
    /// The size, insets and theme the editor remembers.
    #[must_use]
    pub fn view(&self) -> PreviewView {
        PreviewView {
            size: self.size,
            safe_area: self.safe_area,
            theme: self.theme.clone(),
        }
    }

    /// Takes a remembered view; a size or insets that no longer validate (a hand-edited file) leave
    /// the settings as they were. Whether anything was applied.
    pub fn apply_view(&mut self, view: &PreviewView) -> bool {
        let mut next = self.clone();
        next.size = view.size;
        next.safe_area = view.safe_area;
        next.theme.clone_from(&view.theme);
        if next.validate().is_err() {
            return false;
        }
        *self = next;
        true
    }

    /// Applies a named size preset (size and its insets).
    pub fn use_preset(&mut self, name: &str) -> Result<(), DesignerError> {
        let (_, size, safe) = SIZE_PRESETS.iter().find(|(n, ..)| *n == name).ok_or_else(|| {
            let names: Vec<&str> = SIZE_PRESETS.iter().map(|(n, ..)| *n).collect();
            DesignerError::new(
                "bad_size",
                format!("unknown size {name:?}; presets: {}", names.join(", ")),
            )
        })?;
        self.size = *size;
        self.safe_area = *safe;
        Ok(())
    }

    /// The safe-area insets as the runtime's type (whole pixels; ADR-7677).
    #[must_use]
    pub fn safe_area_insets(&self) -> regia_platform::SafeArea {
        let px = |v: f32| v.max(0.0).round() as u32;
        let [l, t, r, b] = self.safe_area;
        regia_platform::SafeArea {
            left: px(l),
            top: px(t),
            right: px(r),
            bottom: px(b),
        }
    }

    /// Sets a custom window size and its safe-area insets (the designer's size controls, ADR-7677):
    /// the insets are limited to the window as the runtime limits them
    /// ([`regia_platform::SafeArea::clamped_to`]), then checked like every size; on an error
    /// nothing changes.
    ///
    /// # Errors
    ///
    /// `bad_size` when the size is outside 1..=[`MAX_PREVIEW_EDGE`] or the insets leave no area.
    pub fn set_window(
        &mut self,
        size: [u32; 2],
        safe: regia_platform::SafeArea,
    ) -> Result<(), DesignerError> {
        let safe = safe.clamped_to(size[0], size[1]);
        let mut next = self.clone();
        next.size = [size[0] as f32, size[1] as f32];
        next.safe_area = [
            safe.left as f32,
            safe.top as f32,
            safe.right as f32,
            safe.bottom as f32,
        ];
        next.validate()?;
        *self = next;
        Ok(())
    }

    /// Checks the size and insets.
    pub fn validate(&self) -> Result<(), DesignerError> {
        if self.scale.is_some_and(|v| !(0.05..=16.0).contains(&v)) || !(24.0..=1536.0).contains(&self.dpi) {
            return Err(DesignerError::new(
                "bad_value",
                "scale must be 0.05..=16; DPI must be 24..=1536",
            ));
        }
        if let Some(mode) = self.scale_mode {
            use regia_ui::ScaleMode;
            let valid = match mode {
                ScaleMode::ScaleWithScreen {
                    reference,
                    match_width,
                } => {
                    reference.iter().all(|v| v.is_finite() && *v > 0.0) && (0.0..=1.0).contains(&match_width)
                }
                ScaleMode::IntegerScale { reference } => reference.iter().all(|v| v.is_finite() && *v > 0.0),
                _ => true,
            };
            if !valid {
                return Err(DesignerError::new(
                    "bad_value",
                    "scale rule needs positive finite reference dimensions and match_width in 0..=1",
                ));
            }
        }
        let [w, h] = self.size;
        if !(1.0..=MAX_PREVIEW_EDGE).contains(&w) || !(1.0..=MAX_PREVIEW_EDGE).contains(&h) {
            return Err(DesignerError::new(
                "bad_size",
                format!("size {w}x{h} is outside 1..={MAX_PREVIEW_EDGE} px"),
            ));
        }
        let [l, t, r, b] = self.safe_area;
        if [l, t, r, b].iter().any(|v| !v.is_finite() || *v < 0.0) || l + r >= w || t + b >= h {
            return Err(DesignerError::new(
                "bad_size",
                "safe-area insets must be >= 0 and leave some area",
            ));
        }
        Ok(())
    }
}

/// One laid-out node.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewNode {
    /// Node path (`"/0/1"`). The rows of a repeat all carry the path of their template node.
    pub path: String,
    /// The sample item this node was generated for; `None` outside a repeat template.
    pub item: Option<usize>,
    /// Node name.
    pub name: Option<String>,
    /// Kind (`button`, `text`, `panel`, …).
    pub kind: &'static str,
    /// Rect: x, y, width, height (physical px from the window's top-left).
    pub rect: [f32; 4],
    /// Clip rect.
    pub clip: [f32; 4],
    /// Shown (it and every ancestor has a display other than `none`).
    pub visible: bool,
    /// Effective runtime layout direction, including ancestor and locale inheritance.
    pub rtl: bool,
    /// Margin then padding bands from the spawned node's effective style, in physical pixels.
    /// Present only when the box-model preview is enabled, including component-inherited styles.
    pub box_model_bands: Option<[Vec<[f32; 4]>; 2]>,
    /// The text it shows (localized spans resolved).
    pub text: Option<String>,
    /// Visible, with area, and not inside the safe area.
    pub outside_safe_area: bool,
}

/// A laid-out document.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewFrame {
    /// Window size.
    pub size: [f32; 2],
    /// Safe-area insets.
    pub safe_area: [f32; 4],
    /// Effective UI scale of the root after the preview rule, project curve or manual override.
    pub scale: f32,
    /// Nodes in tree order (the root first).
    pub nodes: Vec<PreviewNode>,
    /// Sample items were left out to stay under [`MAX_PREVIEW_NODES`].
    pub truncated: bool,
    /// Every repeat on a `list` widget: what the virtualized list materializes (ADR-2715).
    pub lists: Vec<PreviewList>,
}

/// A virtualized repeat in the preview: a `repeat` on a `list` widget, scrolled to the top.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewList {
    /// Path of the list node.
    pub path: String,
    /// Sample items.
    pub items: usize,
    /// Row height (UI px).
    pub row_height: f32,
    /// Rows the runtime materializes for this viewport: the visible rows plus the overscan below.
    pub materialized: usize,
    /// The first visible item's key: its sample key value, else its position.
    pub first_visible_key: Option<String>,
}

/// Rows a virtualized list keeps alive beyond the viewport on each side (the `list` widget's
/// default overscan).
pub const LIST_OVERSCAN: usize = 2;

/// Reads `<project>/assets/localization/<locale>/*.ftl` (locales and files sorted by name).
pub fn project_localization(project_root: &Path) -> Option<Localization> {
    let dir = project_root.join("assets").join("localization");
    let mut locales: Vec<_> = std::fs::read_dir(&dir).ok()?.filter_map(Result::ok).collect();
    locales.sort_by_key(std::fs::DirEntry::file_name);
    let mut sources = Vec::new();
    for entry in locales {
        if !entry.path().is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        let mut files: Vec<_> = files
            .filter_map(Result::ok)
            .map(|f| f.path())
            .filter(|p| p.extension().is_some_and(|e| e == "ftl"))
            .collect();
        files.sort();
        let text: String = files
            .iter()
            .filter_map(|p| std::fs::read_to_string(p).ok())
            .collect::<Vec<_>>()
            .join("\n");
        sources.push((entry.file_name().to_string_lossy().into_owned(), text));
    }
    if sources.is_empty() {
        return None;
    }
    let fallback = sources.first().map(|(l, _)| l.clone());
    match Localization::new(sources, fallback) {
        Ok(loc) => Some(loc),
        Err(e) => {
            tracing::warn!(target: "regia_editor", "ui designer preview: {e}");
            None
        }
    }
}

/// The document with sample values written into the bound properties (text, bars, sliders,
/// toggles, inputs, selections, visibility, enabled state, opacity) and each repeat's template
/// copied once per sample item.
pub fn with_samples(file: &UiFile, samples: &BTreeMap<String, String>) -> UiFile {
    expand(file, samples, DEFAULT_REPEAT_COUNT, None).file
}

/// A document ready to spawn, with where each of its nodes came from.
struct Expanded {
    file: UiFile,
    /// For each node of `file` in depth-first order: its path in the document, and its sample item.
    origins: Vec<(String, Option<usize>)>,
    truncated: bool,
    /// Repeats on a `list` widget, in document order.
    lists: Vec<ExpandedList>,
    /// Where component instances are read from.
    assets_root: Option<std::path::PathBuf>,
    /// Inside a component instance: the instance's document path, which every inherited node
    /// reports as its origin (spec 111 § 6).
    frozen: Option<String>,
    /// Give interactive scratch bindings their exact sample item paths.
    sample_binding_paths: bool,
}

/// One instance node expanded for the preview through the project's `assets/` directory.
pub(crate) fn expand_instance(
    def: &UiNodeDef,
    assets_root: Option<&std::path::Path>,
) -> regia_ui::ui_component::Expansion {
    let migrators = regia_ui::UiMigrators::default();
    let root = assets_root.map(std::path::Path::to_path_buf);
    let mut load = regia_ui::ui_component::text_loader(
        move |p: &str| match &root {
            Some(root) => std::fs::read_to_string(root.join(p)).map_err(|e| format!("{p}: {e}")),
            None => Err(format!(
                "{p}: the document is not under a project's assets/ directory"
            )),
        },
        &migrators,
    );
    regia_ui::ui_component::expand_node(def, "<designer>", &mut load)
}

/// A repeat on a `list` widget as expanded: where its host and each item's nodes are.
struct ExpandedList {
    path: String,
    /// Index of the host in `origins`.
    host: usize,
    row_height: f32,
    first_key: Option<String>,
    /// Per sample item: its range of `origins`.
    items: Vec<std::ops::Range<usize>>,
}

/// The sample paths of the item a template is expanded for.
#[derive(Clone)]
struct ItemScope {
    /// `"Vm.items[1]"`.
    exact: String,
    /// `"Vm.items[]"`.
    any: String,
    index: usize,
}

/// Replaces each repeat by copies of its template and applies the samples.
fn expand_mode(
    file: &UiFile,
    samples: &BTreeMap<String, String>,
    default_count: usize,
    assets_root: Option<&std::path::Path>,
    sample_binding_paths: bool,
) -> Expanded {
    let mut out = Expanded {
        file: UiFile {
            version: file.version,
            component: None,
            root: UiNodeDef::default(),
            variants: Vec::new(),
            back: file.back,
        },
        origins: Vec::new(),
        truncated: false,
        lists: Vec::new(),
        assets_root: assets_root.map(std::path::Path::to_path_buf),
        frozen: None,
        sample_binding_paths,
    };
    let mut budget = MAX_PREVIEW_NODES;
    let root = expand_node(
        &file.root,
        &mut Vec::new(),
        None,
        samples,
        default_count,
        &mut budget,
        &mut out,
    );
    out.file.root = root;
    out
}

fn subtree_size(node: &UiNodeDef) -> usize {
    1 + node.children.iter().map(subtree_size).sum::<usize>()
}

fn expand_node(
    def: &UiNodeDef,
    path: &mut Vec<usize>,
    scope: Option<&ItemScope>,
    samples: &BTreeMap<String, String>,
    default_count: usize,
    budget: &mut usize,
    out: &mut Expanded,
) -> UiNodeDef {
    if def.use_component.is_some() {
        // The instance draws as its component (spec 111 § 6); its nodes are inherited, so they all
        // report the instance as their origin.
        let expanded = expand_instance(def, out.assets_root.as_deref()).root;
        let outermost = out.frozen.is_none();
        if outermost {
            out.frozen = Some(path_string(path));
        }
        let node = expand_node(&expanded, path, scope, samples, default_count, budget, out);
        if outermost {
            out.frozen = None;
        }
        return node;
    }
    *budget = budget.saturating_sub(1);
    let origin = out.origins.len();
    let at = out.frozen.clone().unwrap_or_else(|| path_string(path));
    out.origins.push((at, scope.map(|s| s.index)));
    let mut node = def.clone();
    node.repeat = None;
    node.children = Vec::new();
    let binds: Vec<BindDef> = def.bind.iter().chain(&def.bindings).cloned().collect();
    for bind in binds {
        let raw = match (bind.source.strip_prefix('.'), scope) {
            (Some(rel), Some(scope)) => {
                let suffix = if rel.is_empty() {
                    String::new()
                } else {
                    format!(".{rel}")
                };
                samples
                    .get(&format!("{}{suffix}", scope.exact))
                    .or_else(|| samples.get(&format!("{}{suffix}", scope.any)))
            }
            (Some(_), None) => None,
            (None, _) => samples.get(&bind.source),
        };
        if let Some(raw) = raw {
            apply_sample(&mut node, &bind, raw);
        }
    }
    // A scratch repeat copy keeps its exact sample identity for interactive bindings.
    if out.sample_binding_paths
        && let Some(scope) = scope
    {
        for bind in node.bind.iter_mut().chain(&mut node.bindings) {
            if let Some(relative) = bind.source.strip_prefix('.') {
                bind.source = if relative.is_empty() {
                    scope.exact.clone()
                } else {
                    format!("{}.{relative}", scope.exact)
                };
            }
        }
    }
    if let Some(repeat) = &def.repeat {
        let (exact, any) = match (repeat.source.strip_prefix('.'), scope) {
            (Some(rel), Some(scope)) => {
                let suffix = if rel.is_empty() {
                    String::new()
                } else {
                    format!(".{rel}")
                };
                (
                    format!("{}{suffix}", scope.exact),
                    format!("{}{suffix}", scope.any),
                )
            }
            _ => (repeat.source.clone(), repeat.source.clone()),
        };
        let count = samples
            .get(&format!("{any}[]#count"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(default_count)
            .min(MAX_REPEAT_COUNT);
        let per_item: usize = def.children.iter().map(subtree_size).sum();
        // A repeat on a list is virtualized: each item is a row of the list's row height, or of
        // the template's fixed height with `row_height: template` (ADR-7751).
        let row_height = match def.widget {
            Some(WidgetDef::List(n, h)) => {
                let h = match repeat.row_height {
                    regia_ui::authoring::RepeatRowHeight::Template => {
                        regia_ui::authoring::RepeatDef::template_row_height(&def.children).unwrap_or(h)
                    }
                    regia_ui::authoring::RepeatRowHeight::List => h,
                };
                node.widget = Some(WidgetDef::List(n, h));
                Some(h.max(1.0))
            }
            _ => None,
        };
        let list_index = row_height.map(|h| {
            let key = repeat.key.as_deref().map(|k| k.strip_prefix('.').unwrap_or(k));
            let first_key = match key {
                Some("") => samples
                    .get(&format!("{exact}[0]"))
                    .or_else(|| samples.get(&format!("{any}[]"))),
                Some(k) => samples
                    .get(&format!("{exact}[0].{k}"))
                    .or_else(|| samples.get(&format!("{any}[].{k}"))),
                None => None,
            }
            .cloned();
            out.lists.push(ExpandedList {
                path: path_string(path),
                host: origin,
                row_height: h,
                first_key,
                items: Vec::new(),
            });
            out.lists.len() - 1
        });
        for index in 0..count {
            if per_item > *budget {
                out.truncated = true;
                break;
            }
            let item = ItemScope {
                exact: format!("{exact}[{index}]"),
                any: format!("{any}[]"),
                index,
            };
            let first = out.origins.len();
            for (i, child) in def.children.iter().enumerate() {
                path.push(i);
                let mut copy = expand_node(child, path, Some(&item), samples, default_count, budget, out);
                path.pop();
                if let Some(h) = row_height {
                    place_as_row(&mut copy, index, h);
                }
                node.children.push(copy);
            }
            if let Some(list) = list_index.and_then(|i| out.lists.get_mut(i)) {
                list.items.push(first..out.origins.len());
            }
        }
    } else {
        for (i, child) in def.children.iter().enumerate() {
            path.push(i);
            let copy = expand_node(child, path, scope, samples, default_count, budget, out);
            path.pop();
            node.children.push(copy);
        }
    }
    node
}

/// Puts a template root where a virtualized list puts row `index`: an absolute box at
/// `index * row_height`, as wide as the list and a row tall (a fixed authored height stays; lint
/// `repeat_on_list` reports it).
fn place_as_row(node: &mut UiNodeDef, index: usize, row_height: f32) {
    use regia_ui::{Dimension, Edges, PositionType};
    node.style.position = PositionType::Absolute;
    node.style.inset = Edges {
        left: Dimension::Points(0.0),
        top: Dimension::Points(index as f32 * row_height),
        right: Dimension::Auto,
        bottom: Dimension::Auto,
    };
    if !matches!(node.style.size[0], Dimension::Points(_)) {
        node.style.size[0] = Dimension::Percent(100.0);
    }
    if !matches!(node.style.size[1], Dimension::Points(_)) {
        node.style.size[1] = Dimension::Points(row_height);
    }
}

fn apply_sample(node: &mut UiNodeDef, bind: &BindDef, raw: &str) {
    let is_bool = matches!(
        bind.target,
        BindTargetDef::Checked | BindTargetDef::Visible | BindTargetDef::Enabled
    );
    let value = regia_ui::binding::display_value(
        raw,
        bind.converter.as_ref(),
        bind.format.as_deref(),
        bind.invert && is_bool,
    );
    let loc_key = matches!(bind.converter, Some(BindConverter::LocKey { .. }));
    let number = value.trim().parse::<f32>().ok();
    let flag = value.trim() == "true";
    match (bind.target, node.widget.as_mut()) {
        (BindTargetDef::Text, _) => {
            let text = node.text.get_or_insert_with(|| TextDef {
                spans: Vec::new(),
                align: Default::default(),
                wrap: Default::default(),
                overflow: Default::default(),
                line_height: 1.2,
                style: Default::default(),
                markup: false,
            });
            text.spans = vec![if loc_key {
                TextSpanDef::Loc(LocSpan::Loc(value))
            } else {
                TextSpanDef::Lit(value)
            }];
        }
        (BindTargetDef::Progress, Some(WidgetDef::Progress(v))) => *v = number.unwrap_or(*v).clamp(0.0, 1.0),
        (BindTargetDef::Value, Some(WidgetDef::Slider(v, lo, hi, _))) => {
            *v = number.unwrap_or(*v).clamp(lo.min(*hi), lo.max(*hi));
        }
        (BindTargetDef::Checked, Some(WidgetDef::Toggle(c))) => *c = flag,
        (BindTargetDef::Input, Some(WidgetDef::TextInput(placeholder, ..))) => *placeholder = value,
        (BindTargetDef::Selected, Some(WidgetDef::Dropdown(options, selected))) => {
            if let Ok(i) = value.trim().parse::<usize>()
                && i < options.len()
            {
                *selected = Some(i);
            }
        }
        (BindTargetDef::Selected, Some(WidgetDef::TabBar(tabs, active))) => {
            if let Ok(i) = value.trim().parse::<usize>()
                && i < tabs.len()
            {
                *active = i;
            }
        }
        (BindTargetDef::Opacity, _) => {
            if let Some(n) = number {
                node.opacity = Some(n.clamp(0.0, 1.0));
            }
        }
        (BindTargetDef::Visible, _) => {
            if !flag {
                node.style.display = regia_ui::Display::None;
            }
        }
        (BindTargetDef::Enabled, _) => node.disabled = !flag,
        _ => {}
    }
}

/// The document as the preview spawns it: repeats expanded into `settings.repeat_count` sample
/// items (rows of a virtualized list placed as the list places them) with the samples applied.
pub(crate) fn expanded_document(file: &UiFile, settings: &PreviewSettings) -> UiFile {
    expand(
        &crate::ui_designer_variants::preview_document(file, settings),
        &settings.samples,
        settings.repeat_count,
        settings.assets_root.as_deref(),
    )
    .file
}

/// Read the existing frozen UI settings shape; a missing file is the engine default.
pub(crate) fn read_scale_settings(path: &Path) -> Result<regia_ui::scale::UiSettingsFile, DesignerError> {
    match std::fs::read_to_string(path) {
        Ok(text) => regia_ui::scale::parse_ui_settings_ron(&text, &path.display().to_string())
            .map_err(|e| DesignerError::new("parse", e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(regia_ui::scale::UiSettingsFile {
            version: regia_ui::scale::UI_SETTINGS_VERSION,
            ..Default::default()
        }),
        Err(e) => Err(DesignerError::new("io", format!("{}: {e}", path.display()))),
    }
}

#[derive(Resource)]
struct PreviewScaleMode(regia_ui::ScaleMode);

fn apply_preview_scale(
    mode: Res<PreviewScaleMode>,
    mut roots: Query<&mut regia_ui::UiRoot, Added<regia_ui::UiRoot>>,
) {
    for mut root in &mut roots {
        if root.scale_mode != mode.0 {
            root.scale_mode = mode.0;
        }
    }
}

/// A headless app at the preview's window size, safe area and locale.
pub(crate) fn preview_app(
    settings: &PreviewSettings,
    localization: Option<Localization>,
) -> Result<App, DesignerError> {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    let mode = if settings.scale.is_some() {
        regia_ui::ScaleMode::ConstantPhysicalSize
    } else {
        settings.scale_mode.unwrap_or_default()
    };
    app.insert_resource(PreviewScaleMode(mode));
    app.add_systems(
        regia_app::PostUpdate,
        apply_preview_scale.before(regia_app::PostUpdateSet::UiLayout),
    );

    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.init_resource::<regia_ecs::prelude::AppTypeRegistry>();
    // Startup (fonts, default window size), then the preview's own window.
    app.update();
    let mut loc = localization.unwrap_or_default();
    if let Some(locale) = &settings.locale {
        let _ = loc.set_locale(locale);
    }
    let _ = loc.set_pseudo(settings.pseudo);
    let world = app.world_mut();
    world.insert_resource(loc);
    world.insert_resource(regia_ui::UiScales {
        window_size: settings.size,
        scale_factor: f64::from(settings.scale.unwrap_or(settings.dpi / 96.0)),
    });
    if settings.scale.is_none()
        && let Some(assets) = &settings.assets_root
    {
        let file = read_scale_settings(&assets.join("ui/project.uisettings.ron"))?;
        // A preview override that is not the engine default wins: `apply_project_scale_mode`
        // only replaces roots still on that default. An explicit default cannot be told from
        // unset, so a saved rule is applied only when the preview has no rule of its own.
        let project_mode = settings.scale_mode.is_none().then_some(file.scale_mode).flatten();
        world.insert_resource(regia_ui::scale::UiScaleSettings {
            dpi_curve: file.dpi_curve.map(|points| regia_ui::scale::DpiCurve { points }),
            project_mode,
        });
    }
    let [l, t, r, b] = settings.safe_area;
    world.insert_resource(regia_platform::SafeArea {
        left: l as u32,
        top: t as u32,
        right: r as u32,
        bottom: b as u32,
    });
    Ok(app)
}

/// Lays the document out at `settings` and returns every node's rect. `localization` resolves
/// localized spans (missing keys show as `⟦key⟧`).
pub fn preview(
    file: &UiFile,
    settings: &PreviewSettings,
    localization: Option<Localization>,
) -> Result<PreviewFrame, DesignerError> {
    let _span = tracing::info_span!("ui_designer_preview").entered();
    settings.validate()?;
    // The layout variant being edited, or the one the preview size picks (ADR-7858).
    let shown = crate::ui_designer_variants::preview_document(file, settings);
    let file: &UiFile = &shown;
    let Expanded {
        file,
        origins,
        truncated,
        lists,
        ..
    } = expand(
        file,
        &settings.samples,
        settings.repeat_count,
        settings.assets_root.as_deref(),
    );
    let mut app = preview_app(settings, localization)?;
    let world = app.world_mut();
    const NAME: &str = "__ui_designer_preview";
    // Widgets add runtime children of their own, so entities are found by a private name instead
    // of by their position in the tree.
    let mut tagged = file.clone();
    tag_nodes(&mut tagged.root, &mut 0);
    // Pictures named by asset path or icon take designer ids: the layout needs no asset server
    // (ADR-3148).
    crate::ui_designer_images::substitute_pictures(&mut tagged, None);
    world
        .resource_mut::<UiLibrary>()
        .templates
        .insert(NAME.to_owned(), tagged);
    let root =
        regia_ui::authoring::spawn_ui(world, NAME).map_err(|e| DesignerError::new("parse", e.to_string()))?;
    // Game widgets get invented rows, so the layout shows what a match would.
    regia_ui::game_widgets::fill_preview_samples(app.world_mut());
    app.update();
    app.update();

    let tagged: BTreeMap<usize, Entity> = {
        let world = app.world_mut();
        let mut names = world.query::<(Entity, &NodeName)>();
        names
            .iter(world)
            .filter_map(|(entity, name)| Some((name.0.strip_prefix(TAG)?.parse().ok()?, entity)))
            .collect()
    };
    let world = app.world();
    let mut defs = Vec::new();
    walk(&file.root, &mut Vec::new(), &mut |node, path| {
        defs.push((node.name.clone(), node_kind(node), path.len()));
    });
    let loc = world.get_resource::<Localization>();
    let scale = world.get::<ComputedNode>(root).map_or(1.0, |c| c.scale);
    let [l, t, r, b] = settings.safe_area;
    let safe = [l, t, settings.size[0] - r, settings.size[1] - b];
    let mut nodes = Vec::with_capacity(defs.len());
    let mut hidden_below: Option<usize> = None;
    for (index, ((name, kind, depth), (path, item))) in defs.into_iter().zip(origins).enumerate() {
        // `spawn_ui` keeps the root's entity, not its name.
        let entity = if depth == 0 {
            Some(root)
        } else {
            tagged.get(&index).copied()
        };
        let Some(entity) = entity else {
            return Err(DesignerError::new("parse", format!("node {path} did not spawn")));
        };
        if hidden_below.is_some_and(|d| depth <= d) {
            hidden_below = None;
        }
        let shown = world
            .get::<NodeStyle>(entity)
            .is_none_or(|s| s.0.display != regia_ui::Display::None);
        if !shown && hidden_below.is_none() {
            hidden_below = Some(depth);
        }
        let visible = hidden_below.is_none();
        let computed = world.get::<ComputedNode>(entity).copied().unwrap_or_default();
        let [x, y, w, h] = computed.rect;
        // The root fills the window by design; only the nodes below it are checked.
        let outside = depth > 0
            && visible
            && w > 0.0
            && h > 0.0
            && (x < safe[0] - 0.5 || y < safe[1] - 0.5 || x + w > safe[2] + 0.5 || y + h > safe[3] + 0.5);
        let text = world.get::<Text>(entity).map(|t| {
            t.spans
                .iter()
                .map(|s| match &s.content {
                    TextContent::Lit(s) => s.clone(),
                    TextContent::Loc { key, args } => match loc {
                        Some(loc) => loc.resolve(key, args),
                        None => format!("\u{27E6}{key}\u{27E7}"),
                    },
                })
                .collect::<String>()
        });
        let rtl = regia_ui::layout::resolved_rtl(
            world,
            entity,
            loc.is_some_and(|loc| regia_ui::style::locale_is_rtl(loc.locale())),
        );
        let box_model_bands = settings.box_model.then(|| {
            let style = world.get::<NodeStyle>(entity).map(|style| &style.0);
            style.map_or_else(
                || [Vec::new(), Vec::new()],
                |style| crate::ui_designer_canvas::margin_padding_bands(computed.rect, computed.scale, style),
            )
        });
        nodes.push(PreviewNode {
            rtl,
            box_model_bands,
            path,
            item,
            name,
            kind,
            rect: computed.rect,
            clip: computed.clip_rect,
            visible,
            text,
            outside_safe_area: outside,
        });
    }
    // Virtualized lists: the rows past the materialized range do not exist in a game.
    let mut preview_lists = Vec::with_capacity(lists.len());
    for list in lists {
        let entity = if list.host == 0 {
            Some(root)
        } else {
            tagged.get(&list.host).copied()
        };
        let computed = entity
            .and_then(|e| world.get::<ComputedNode>(e).copied())
            .unwrap_or_default();
        let s = computed.scale.max(0.001);
        let geometry =
            regia_ui::ListGeometry::Rows(regia_ui::RowMetrics::uniform(list.items.len(), list.row_height));
        let materialized = if computed.rect[3] > 0.0 {
            geometry
                .visible_range(0.0, computed.rect[3] / s, LIST_OVERSCAN)
                .len()
        } else {
            0
        };
        for range in list.items.iter().skip(materialized) {
            for node in nodes.get_mut(range.clone()).into_iter().flatten() {
                node.visible = false;
                node.outside_safe_area = false;
            }
        }
        preview_lists.push(PreviewList {
            path: list.path,
            items: list.items.len(),
            row_height: list.row_height,
            materialized,
            first_visible_key: (!list.items.is_empty())
                .then(|| list.first_key.unwrap_or_else(|| "0".to_owned())),
        });
    }
    Ok(PreviewFrame {
        size: settings.size,
        safe_area: settings.safe_area,
        scale,
        nodes,
        truncated,
        lists: preview_lists,
    })
}

/// The private name prefix of the preview's nodes.
const TAG: &str = "__ui_designer_node_";

/// Names every node `TAG<n>`, `n` counting depth first (the order of `walk`).
fn tag_nodes(node: &mut UiNodeDef, next: &mut usize) {
    node.name = Some(format!("{TAG}{next}"));
    *next += 1;
    for child in &mut node.children {
        tag_nodes(child, next);
    }
}

/// Expanded scratch data shared by the interactive runtime and its source inspector.
pub(crate) struct InteractiveDocument {
    /// The private expanded document, with exact sample paths.
    pub file: UiFile,
    /// Authored paths and repeated item indices in preorder.
    pub origins: Vec<(String, Option<usize>)>,
    /// List host and expanded item ranges in the same preorder.
    pub lists: Vec<(usize, Vec<std::ops::Range<usize>>)>,
    /// The shared preview expansion budget omitted some repeated items.
    pub truncated: bool,
}

/// Expands existing samples while preserving per-item binding identities for playback.
pub(crate) fn interactive_document(file: &UiFile, settings: &PreviewSettings) -> InteractiveDocument {
    let expanded = expand_mode(
        file,
        &settings.samples,
        settings.repeat_count,
        settings.assets_root.as_deref(),
        true,
    );
    InteractiveDocument {
        file: expanded.file,
        origins: expanded.origins,
        lists: expanded
            .lists
            .into_iter()
            .map(|list| (list.host, list.items))
            .collect(),
        truncated: expanded.truncated,
    }
}

fn expand(
    file: &UiFile,
    samples: &BTreeMap<String, String>,
    default_count: usize,
    assets_root: Option<&std::path::Path>,
) -> Expanded {
    expand_mode(file, samples, default_count, assets_root, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ui::{UiMigrators, parse_ui_ron};

    const DOC: &str = r#"(version: 3, root: (
        style: (direction: column, size: (Percent(100), Percent(100))),
        children: [
            (name: "top", style: (size: (Percent(100), 50))),
            (name: "hp", widget: progress(0.0), style: (size: (200, 20)),
                bindings: [(source: "HudVm.health", target: progress), (source: "HudVm.dead", target: visible, invert: true)]),
            (name: "label", text: (spans: [Loc("menu-start")]), style: (size: (300, 40))),
            (name: "score", text: (spans: ["-"]), bindings: [(source: "Score.value", format: "{:03}")]),
        ],
    ))"#;

    fn file() -> UiFile {
        parse_ui_ron(DOC, "t", &UiMigrators::default()).expect("doc")
    }

    #[test]
    fn box_bands_use_component_inherited_spacing() {
        let assets = regia_test_utils::unique_temp_dir("designer-component-box-bands");
        std::fs::create_dir_all(assets.join("ui")).expect("fixture directory");
        let source = format!(
            "(version: {}, component: (params: []), root: (style: (size: (100,80), padding: (7,7,7,7), margin: (3,3,3,3))))",
            regia_ui::UI_RON_VERSION,
        );
        regia_test_utils::write_atomic(&assets.join("ui/box.ui.ron"), source.as_bytes()).expect("component");
        let component =
            parse_ui_ron(&source, "component", &UiMigrators::default()).expect("component definition");
        let mut doc = file();
        doc.root.children = vec![UiNodeDef {
            use_component: Some("ui/box.ui.ron".into()),
            ..Default::default()
        }];
        let settings = PreviewSettings {
            assets_root: Some(assets.clone()),
            scale: Some(1.5),
            box_model: true,
            ..Default::default()
        };
        let frame = preview(&doc, &settings, None).expect("component preview");
        let node = frame
            .nodes
            .iter()
            .find(|node| node.path == "/0")
            .expect("instance");
        let bands = node.box_model_bands.as_ref().expect("effective box model");
        assert_eq!(
            bands,
            &crate::ui_designer_canvas::margin_padding_bands(node.rect, frame.scale, &component.root.style)
        );
        assert_ne!(
            bands,
            &crate::ui_designer_canvas::margin_padding_bands(
                node.rect,
                frame.scale,
                &doc.root.children[0].style
            )
        );
        std::fs::remove_dir_all(assets).expect("fixture cleanup");
    }

    #[test]
    fn rects_follow_the_window_size_and_flag_the_safe_area() {
        let mut settings = PreviewSettings::default();
        settings.use_preset("1920x1080").expect("preset");
        let wide = preview(&file(), &settings, None).expect("preview");
        assert_eq!(wide.nodes.len(), 5);
        assert_eq!(wide.nodes[1].name.as_deref(), Some("top"));
        assert_eq!(wide.nodes[1].rect[2], 1920.0, "{:?}", wide.nodes[1]);
        assert!(wide.nodes.iter().all(|n| !n.outside_safe_area));

        settings.use_preset("2400x1080").expect("preset");
        let phone = preview(&file(), &settings, None).expect("preview");
        assert_eq!(phone.nodes[1].rect[2], 2400.0);
        // The full-width bar crosses the left cutout inset.
        assert!(phone.nodes[1].outside_safe_area, "{:?}", phone.nodes[1]);
        assert_eq!(phone.nodes[1].path, "/0");
    }

    #[test]
    fn samples_and_localization_fill_the_preview() {
        let mut settings = PreviewSettings::default();
        settings.samples.insert("Score.value".into(), "7".into());
        settings.samples.insert("HudVm.dead".into(), "true".into());
        let loc = || Localization::new(vec![("en".into(), "menu-start = Start".into())], None).expect("ok");
        let frame = preview(&file(), &settings, Some(loc())).expect("preview");
        let by_name = |n: &str| {
            frame
                .nodes
                .iter()
                .find(|p| p.name.as_deref() == Some(n))
                .expect("node in frame")
        };
        assert_eq!(by_name("score").text.as_deref(), Some("007"));
        assert_eq!(by_name("label").text.as_deref(), Some("Start"));
        assert!(
            !by_name("hp").visible,
            "dead = true hides the bar (visible is inverted)"
        );

        settings.pseudo = true;
        let pseudo = preview(&file(), &settings, Some(loc())).expect("preview");
        let label = pseudo
            .nodes
            .iter()
            .find(|p| p.name.as_deref() == Some("label"))
            .expect("label in frame");
        assert_ne!(label.text.as_deref(), Some("Start"), "pseudo text differs");
        let missing = preview(&file(), &PreviewSettings::default(), None).expect("preview");
        let label = missing
            .nodes
            .iter()
            .find(|p| p.name.as_deref() == Some("label"))
            .expect("label in frame");
        assert!(label.text.as_deref().is_some_and(|t| t.contains("menu-start")));
    }

    /// Every kind of widget, with a named marker after each: widgets that add runtime children
    /// (list rows, scrollbars) must not shift the nodes after them.
    const WIDGETS: &str = r#"(version: 3, root: (
        style: (direction: column, size: (Percent(100), Percent(100))),
        children: [
            (name: "list", widget: list(6, 32.0), style: (size: (300, 120))),
            (name: "after_list", style: (size: (100, 40))),
            (name: "scroll", widget: scroll(0.3, true), style: (size: (300, 90)),
                children: [(name: "in_scroll", style: (size: (280, 300)))]),
            (name: "after_scroll", style: (size: (100, 60))),
            (name: "drop", widget: dropdown(["A", "B"], 0), style: (size: (160, 30))),
            (name: "tabs", widget: tab_bar(["One", "Two"], 1), style: (size: (200, 30))),
            (name: "menu", widget: popup_menu([]), style: (size: (80, 30))),
            (name: "slide", widget: slider(0.5, 0.0, 1.0, 0.0), style: (size: (200, 24))),
            (name: "input", widget: text_input("Name", false), style: (size: (200, 30))),
            (name: "last", style: (size: (100, 80))),
        ],
    ))"#;

    /// The same file spawned the way a game does, without the preview's tags.
    fn runtime_rects(file: &UiFile, settings: &PreviewSettings) -> BTreeMap<String, [f32; 4]> {
        let mut app = preview_app(settings, None).expect("preview app");
        let world = app.world_mut();
        world
            .resource_mut::<UiLibrary>()
            .templates
            .insert("game".to_owned(), file.clone());
        regia_ui::authoring::spawn_ui(world, "game").expect("spawn");
        app.update();
        app.update();
        let world = app.world_mut();
        let mut names = world.query::<(&NodeName, &ComputedNode)>();
        names
            .iter(world)
            .map(|(name, computed)| (name.0.clone(), computed.rect))
            .collect()
    }

    #[test]
    fn rects_match_the_runtime_layout_with_runtime_children_present() {
        let file = parse_ui_ron(WIDGETS, "widgets.ui.ron", &UiMigrators::default()).expect("parse");
        let mut settings = PreviewSettings::default();
        settings.use_preset("1280x800").expect("preset");
        let frame = preview(&file, &settings, None).expect("preview");
        let runtime = runtime_rects(&file, &settings);
        let named: Vec<_> = frame.nodes.iter().filter(|n| n.name.is_some()).collect();
        assert_eq!(named.len(), 11, "every authored node, and only those");
        for node in named {
            let name = node.name.as_deref().expect("named");
            assert_eq!(Some(&node.rect), runtime.get(name), "{name}");
        }
        let rect = |name: &str| {
            frame
                .nodes
                .iter()
                .find(|n| n.name.as_deref() == Some(name))
                .expect("node")
                .rect
        };
        // The nodes after the runtime children are the right ones. Rects snap to whole physical
        // pixels, so the heights are far apart and the tolerance is one physical pixel.
        for (name, height) in [("after_list", 40.0), ("after_scroll", 60.0), ("last", 80.0)] {
            let shown = rect(name)[3] / frame.scale;
            assert!((shown - height).abs() <= 1.0 / frame.scale, "{name}: {shown}");
        }
    }

    /// A designer sees invented rows, and the runtime's own children do not confuse the rects
    /// (ADR-2958).
    #[test]
    fn game_widget_presets_preview_with_sample_rows() {
        let mut root = UiNodeDef::default();
        for name in crate::ui_designer::PRESETS
            .iter()
            .filter(|n| ["lobby_list", "scoreboard", "radar", "minimap", "marker_layer"].contains(n))
        {
            root.children
                .push(crate::ui_designer::preset(name).expect("preset"));
        }
        let file = UiFile {
            version: regia_ui::UI_RON_VERSION,
            component: None,
            root,
            variants: Vec::new(),
            back: None,
        };
        let frame = preview(&file, &PreviewSettings::default(), None).expect("previews");
        assert_eq!(
            frame.nodes.len(),
            6,
            "the root and the five widgets, not their runtime rows"
        );
        let kinds: Vec<&str> = frame.nodes.iter().map(|n| n.kind).collect();
        assert!(
            kinds.contains(&"minimap") && kinds.contains(&"lobby_list"),
            "{kinds:?}"
        );
        let radar = frame.nodes.iter().find(|n| n.kind == "radar").expect("radar");
        assert_eq!(
            (radar.rect[2] / frame.scale, radar.rect[3] / frame.scale),
            (200.0, 200.0),
            "{radar:?}"
        );
    }

    /// Spec 81 § 4.2 (and ADR-5814 for `1280x720`): the size presets, the notch insets on the
    /// phone presets, the default and largest sample-item counts, and a missing locale key shown
    /// as `⟦key⟧` (dev builds, as the runtime's `Localization::resolve`).
    #[test]
    fn presets_and_defaults_follow_the_spec() {
        let names: Vec<&str> = SIZE_PRESETS.iter().map(|(n, ..)| *n).collect();
        assert_eq!(
            names,
            ["1920x1080", "1280x720", "1280x800", "2400x1080", "1080x2400"]
        );
        for (name, size, safe) in SIZE_PRESETS {
            let mut settings = PreviewSettings::default();
            settings.use_preset(name).expect("preset");
            assert_eq!((settings.size, settings.safe_area), (size, safe), "{name}");
            let notch = name == "2400x1080" || name == "1080x2400";
            assert_eq!(safe.iter().any(|&inset| inset > 0.0), notch, "{name}");
            settings.validate().expect("a preset is valid");
        }
        assert_eq!(DEFAULT_REPEAT_COUNT, 3);
        assert_eq!(MAX_REPEAT_COUNT, 256);
        assert_eq!(MAX_PREVIEW_NODES, 8192);
        assert_eq!(PreviewSettings::default().repeat_count, DEFAULT_REPEAT_COUNT);
        if cfg!(debug_assertions) {
            let loc = || Localization::new(vec![("en".into(), "other = Other".into())], None).expect("ok");
            let frame = preview(&file(), &PreviewSettings::default(), Some(loc())).expect("preview");
            let label = frame
                .nodes
                .iter()
                .find(|p| p.name.as_deref() == Some("label"))
                .expect("label in frame");
            assert_eq!(label.text.as_deref(), Some("⟦menu-start⟧"));
        }
    }

    #[test]
    fn preview_is_deterministic_and_rejects_bad_sizes() {
        let settings = PreviewSettings::default();
        let a = preview(&file(), &settings, None).expect("preview");
        let b = preview(&file(), &settings, None).expect("preview");
        assert_eq!(a, b);
        let mut bad = PreviewSettings {
            size: [0.0, 100.0],
            ..PreviewSettings::default()
        };
        assert_eq!(
            preview(&file(), &bad, None).expect_err("must fail").code,
            "bad_size"
        );
        bad.size = [100.0, 100.0];
        bad.safe_area = [60.0, 0.0, 60.0, 0.0];
        assert_eq!(bad.validate().expect_err("must fail").code, "bad_size");
        assert_eq!(
            PreviewSettings::default()
                .use_preset("nope")
                .expect_err("must fail")
                .code,
            "bad_size"
        );
    }

    #[test]
    fn designer_preview_is_pure_across_presets_samples_locales_and_invalid_settings() {
        let source = r#"(version: 6, root: (
            name: "same", style: (direction: column, size: (Percent(100), Percent(100))),
            children: [
                (name: "same", style: (size: (Percent(100), 28))),
                (name: "hidden", style: (display: none, size: (Percent(100), 80)), children: [
                    (name: "same", style: (size: (2000, 50))),
                ]),
                (name: "rows", repeat: (source: "Vm.items", key: "id"), children: [
                    (name: "same", text: (spans: [Loc("hello")]), style: (size: (260, 42)),
                        bindings: [(source: ".name", target: text)]),
                ]),
            ],
        ))"#;
        let document =
            parse_ui_ron(source, "preview-purity.ui.ron", &UiMigrators::default()).expect("fixture parses");
        let mut settings_a = PreviewSettings::default();
        settings_a.use_preset("1920x1080").expect("preset A");
        settings_a.samples.insert("Vm.items[]#count".into(), "2".into());
        settings_a
            .samples
            .insert("Vm.items[].name".into(), "A item".into());
        settings_a
            .samples
            .insert("Vm.items[1].name".into(), "second".into());
        settings_a.locale = Some("en".into());
        let mut settings_b = PreviewSettings::default();
        settings_b.use_preset("2400x1080").expect("preset B");
        settings_b.samples.insert("Vm.items[]#count".into(), "3".into());
        settings_b
            .samples
            .insert("Vm.items[].name".into(), "B item".into());
        settings_b.locale = Some("fr".into());
        let invalid = PreviewSettings {
            size: [20.0, 20.0],
            safe_area: [10.0, 0.0, 10.0, 0.0],
            ..PreviewSettings::default()
        };
        let original_text = regia_ui::authoring::write_ui_ron(&document).expect("serialize fixture");
        let original_settings_a = settings_a.clone();
        let original_settings_b = settings_b.clone();
        let original_invalid = invalid.clone();
        let localization = |greeting: &str| {
            Localization::new(
                vec![
                    ("en".into(), format!("hello = {greeting}")),
                    ("fr".into(), "hello = Bonjour".into()),
                ],
                Some("en".into()),
            )
            .expect("localization")
        };
        let frame_a = preview(&document, &settings_a, Some(localization("Hello"))).expect("frame A");
        assert_eq!(
            regia_ui::authoring::write_ui_ron(&document).expect("source after A"),
            original_text
        );
        assert_eq!(settings_a, original_settings_a);
        let frame_b = preview(&document, &settings_b, Some(localization("Different"))).expect("frame B");
        assert_eq!(
            regia_ui::authoring::write_ui_ron(&document).expect("source after B"),
            original_text
        );
        assert_eq!(settings_b, original_settings_b);
        assert_eq!(frame_b.size, [2400.0, 1080.0]);
        assert!(preview(&document, &invalid, Some(localization("Unused"))).is_err());
        assert_eq!(
            regia_ui::authoring::write_ui_ron(&document).expect("source after invalid"),
            original_text
        );
        assert_eq!(invalid, original_invalid);
        let frame_a_again =
            preview(&document, &settings_a, Some(localization("Hello"))).expect("frame A again");
        assert_eq!(frame_a_again, frame_a);
        assert_eq!(
            regia_ui::authoring::write_ui_ron(&document).expect("source after A again"),
            original_text
        );
        assert_eq!(settings_a, original_settings_a);

        let repeated: Vec<_> = frame_a
            .nodes
            .iter()
            .filter(|n| n.name.as_deref() == Some("same"))
            .collect();
        assert_eq!(
            repeated
                .iter()
                .map(|n| (n.path.as_str(), n.item))
                .collect::<Vec<_>>(),
            [
                ("/", None),
                ("/0", None),
                ("/1/0", None),
                ("/2/0", Some(0)),
                ("/2/0", Some(1))
            ]
        );
        assert_eq!(repeated[3].text.as_deref(), Some("A item"));
        let hidden = frame_a
            .nodes
            .iter()
            .find(|n| n.path == "/1/0")
            .expect("hidden descendant");
        assert!(!hidden.visible);
        assert!(
            !hidden.outside_safe_area,
            "hidden descendants are excluded from safe-area warnings"
        );
    }

    #[test]
    fn safe_area_flags_use_rectangle_edges_tolerance_and_root_exemption() {
        let source = r#"(version: 3, root: (style: (direction: column, size: (Percent(100), Percent(100))), children: [
            (name: "bar", style: (size: (Percent(100), 20))),
        ]))"#;
        let document =
            parse_ui_ron(source, "safe-area.ui.ron", &UiMigrators::default()).expect("fixture parses");
        let settings = PreviewSettings {
            size: [100.0, 120.0],
            safe_area: [0.5, 0.0, 0.0, 0.0],
            ..PreviewSettings::default()
        };
        let frame = preview(&document, &settings, None).expect("preview");
        assert!(
            !frame.nodes[0].outside_safe_area,
            "the root fills the window and is exempt"
        );
        let [x, y, w, h] = frame.nodes[1].rect;
        let [l, t, r, b] = settings.safe_area;
        let safe = [l, t, settings.size[0] - r, settings.size[1] - b];
        let expected =
            x < safe[0] - 0.5 || y < safe[1] - 0.5 || x + w > safe[2] + 0.5 || y + h > safe[3] + 0.5;
        assert_eq!(
            frame.nodes[1].outside_safe_area, expected,
            "rect {:?}",
            frame.nodes[1].rect
        );
        assert!(
            !frame.nodes[1].outside_safe_area,
            "0.5 px tolerance admits the near edge: {:?}",
            frame.nodes[1]
        );
        let mut exact_edge = settings.clone();
        exact_edge.safe_area[0] = 0.0;
        let exact = preview(&document, &exact_edge, None).expect("exact edge preview");
        assert!(
            !exact.nodes[1].outside_safe_area,
            "the rectangle exactly at the inset is safe"
        );
        let mut beyond_edge = settings;
        beyond_edge.safe_area[0] = 1.0;
        let outside = preview(&document, &beyond_edge, None).expect("outside edge preview");
        assert!(
            outside.nodes[1].outside_safe_area,
            "one pixel beyond the inset is flagged"
        );
    }

    const LIST: &str = r#"(version: 9, root: (
        style: (direction: column, size: (Percent(100), Percent(100))),
        children: [
            (name: "rows", style: (direction: column), repeat: (source: "Vm.items", key: "id"), children: [
                (name: "row", style: (size: (200, 30)), children: [
                    (name: "label", text: (spans: ["-"]), bindings: [(source: ".name")]),
                    (name: "score", text: (spans: ["-"]), bindings: [(source: ".score", converter: number(precision: 1))]),
                ]),
            ]),
            (name: "after", style: (size: (100, 10))),
        ],
    ))"#;

    #[test]
    fn a_repeat_shows_sample_items_laid_out_in_rows() {
        let file = parse_ui_ron(LIST, "t", &UiMigrators::default()).expect("parse");
        let mut settings = PreviewSettings::default();
        settings.samples.insert("Vm.items[].name".into(), "Sword".into());
        settings
            .samples
            .insert("Vm.items[1].name".into(), "Shield".into());
        settings.samples.insert("Vm.items[].score".into(), "2.25".into());
        let frame = preview(&file, &settings, None).expect("preview");
        assert!(!frame.truncated);
        let rows: Vec<&PreviewNode> = frame
            .nodes
            .iter()
            .filter(|n| n.name.as_deref() == Some("row"))
            .collect();
        assert_eq!(rows.len(), DEFAULT_REPEAT_COUNT);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.path, "/0/0", "every row maps to its template node");
            assert_eq!(row.item, Some(i));
        }
        assert!(rows[1].rect[1] > rows[0].rect[1], "the rows stack: {rows:?}");
        let labels: Vec<&str> = frame
            .nodes
            .iter()
            .filter(|n| n.name.as_deref() == Some("label"))
            .filter_map(|n| n.text.as_deref())
            .collect();
        assert_eq!(labels, ["Sword", "Shield", "Sword"]);
        let score = frame
            .nodes
            .iter()
            .find(|n| n.name.as_deref() == Some("score"))
            .expect("score");
        assert_eq!(
            score.text.as_deref(),
            Some("2.2"),
            "the converter runs on samples"
        );
        let after = frame
            .nodes
            .iter()
            .find(|n| n.name.as_deref() == Some("after"))
            .expect("after");
        assert_eq!(after.path, "/1");
        assert_eq!(after.item, None);
        assert!(after.rect[1] >= rows[2].rect[1] + rows[2].rect[3] - 1.0);
    }

    #[test]
    fn the_item_count_comes_from_the_settings_or_a_sample() {
        let file = parse_ui_ron(LIST, "t", &UiMigrators::default()).expect("parse");
        let count = |frame: &PreviewFrame| {
            frame
                .nodes
                .iter()
                .filter(|n| n.name.as_deref() == Some("row"))
                .count()
        };
        let mut settings = PreviewSettings {
            repeat_count: 5,
            ..PreviewSettings::default()
        };
        assert_eq!(count(&preview(&file, &settings, None).expect("preview")), 5);
        settings.samples.insert("Vm.items[]#count".into(), "2".into());
        assert_eq!(count(&preview(&file, &settings, None).expect("preview")), 2);
        settings.samples.insert("Vm.items[]#count".into(), "0".into());
        assert_eq!(count(&preview(&file, &settings, None).expect("preview")), 0);
    }

    #[test]
    fn nested_repeats_use_nested_sample_paths_and_stay_bounded() {
        let src = r#"(version: 9, root: (children: [
            (repeat: (source: "Vm.sections"), children: [
                (name: "title", text: (spans: ["-"]), bindings: [(source: ".title")]),
                (name: "inner", repeat: (source: ".items"), children: [
                    (name: "cell", text: (spans: ["-"]), bindings: [(source: ".name")]),
                ]),
            ]),
        ]))"#;
        let file = parse_ui_ron(src, "t", &UiMigrators::default()).expect("parse");
        let mut settings = PreviewSettings::default();
        settings.samples.insert("Vm.sections[]#count".into(), "2".into());
        settings
            .samples
            .insert("Vm.sections[].items[]#count".into(), "4".into());
        settings
            .samples
            .insert("Vm.sections[1].title".into(), "Second".into());
        settings
            .samples
            .insert("Vm.sections[].items[].name".into(), "n".into());
        let frame = preview(&file, &settings, None).expect("preview");
        let by = |n: &str| {
            frame
                .nodes
                .iter()
                .filter(|p| p.name.as_deref() == Some(n))
                .collect::<Vec<_>>()
        };
        assert_eq!(by("title").len(), 2);
        assert_eq!(by("cell").len(), 8);
        assert_eq!(by("title")[1].text.as_deref(), Some("Second"));
        assert_eq!(by("cell")[0].text.as_deref(), Some("n"));

        settings
            .samples
            .insert("Vm.sections[]#count".into(), "256".into());
        settings
            .samples
            .insert("Vm.sections[].items[]#count".into(), "256".into());
        let huge = preview(&file, &settings, None).expect("bounded");
        assert!(huge.truncated);
        assert!(huge.nodes.len() <= MAX_PREVIEW_NODES);
    }

    #[test]
    fn the_preview_of_a_file_without_repeats_is_unchanged() {
        let frame = preview(&file(), &PreviewSettings::default(), None).expect("preview");
        assert!(frame.nodes.iter().all(|n| n.item.is_none()));
        let paths: Vec<&str> = frame.nodes.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(paths, ["/", "/0", "/1", "/2", "/3"]);
    }

    /// A repeat on a list is virtualized in the preview as in a game: rows sit at `index * row
    /// height`, only the rows the list materializes at the top count as shown, and `lists` reports
    /// the count and the first visible key (ADR-2715).
    #[test]
    fn a_repeat_on_a_list_shows_only_the_materialized_rows() {
        let src = r#"(version: 6, root: (
            style: (direction: column, size: (Percent(100), Percent(100))),
            children: [
                (name: "feed", widget: list(0, 32.0), style: (size: (300, 160)), clip: true,
                    repeat: (source: "Vm.items", key: "id"),
                    children: [(name: "row", text: (spans: ["-"]), bindings: [(source: ".name")])]),
                (name: "plain", repeat: (source: "Vm.items"), children: [(name: "cell")]),
            ],
        ))"#;
        let file = parse_ui_ron(src, "t.ui.ron", &UiMigrators::default()).expect("parse");
        let mut settings = PreviewSettings {
            repeat_count: 50,
            ..PreviewSettings::default()
        };
        settings
            .samples
            .insert("Vm.items[0].id".to_owned(), "7".to_owned());
        settings
            .samples
            .insert("Vm.items[].name".to_owned(), "Sword".to_owned());
        let frame = preview(&file, &settings, None).expect("preview");
        assert_eq!(frame.lists.len(), 1, "only the list repeat is virtualized");
        let list = &frame.lists[0];
        assert_eq!(list.path, "/0");
        assert_eq!(list.items, 50);
        assert_eq!(list.row_height, 32.0);
        // 160 / 32 = 5 rows, the row the bottom edge touches, and 2 rows of overscan below.
        assert_eq!(list.materialized, 5 + 1 + LIST_OVERSCAN);
        assert_eq!(list.first_visible_key.as_deref(), Some("7"));

        let rows: Vec<&PreviewNode> = frame.nodes.iter().filter(|n| n.path == "/0/0").collect();
        assert_eq!(rows.len(), 50);
        let feed = frame.nodes.iter().find(|n| n.path == "/0").expect("feed").rect;
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.item, Some(i));
            assert_eq!(row.visible, i < list.materialized, "row {i}");
            let want = feed[1] + i as f32 * 32.0 * frame.scale;
            assert!(
                (row.rect[1] - want).abs() <= 1.0,
                "row {i}: {} vs {want}",
                row.rect[1]
            );
            assert!((row.rect[3] - 32.0 * frame.scale).abs() <= 1.0, "row {i} height");
        }
        assert_eq!(rows[0].text.as_deref(), Some("Sword"));
        // The ordinary repeat keeps every sample row shown.
        let cells = frame.nodes.iter().filter(|n| n.path == "/1/0");
        assert!(cells.clone().count() == 50 && cells.into_iter().all(|n| n.visible));
    }

    use regia_app::prelude::{Reflect, ReflectDefault, ReflectResource};
    use regia_ecs::prelude::*;

    #[derive(Reflect, Default, Clone)]
    #[reflect(Default)]
    struct FeedRow {
        id: u32,
        name: String,
    }

    #[derive(Resource, Reflect, Default, Clone)]
    #[reflect(Resource, Default)]
    struct FeedVm {
        items: Vec<FeedRow>,
    }

    /// The preview's rows of a list repeat sit where the runtime's virtualized rows do, and the
    /// preview shows as many as the runtime materializes.
    #[test]
    fn list_repeat_rows_match_the_runtime_virtualized_rows() {
        let src = r#"(version: 6, root: (
            style: (direction: column, size: (Percent(100), Percent(100))),
            children: [
                (name: "top", style: (size: (100, 50))),
                (name: "feed", widget: list(0, 40.0), style: (size: (300, 200)), clip: true,
                    repeat: (source: "FeedVm.items", key: "id"),
                    children: [(name: "row", style: (size: (Percent(100), Percent(100))))]),
            ],
        ))"#;
        let file = parse_ui_ron(src, "t.ui.ron", &UiMigrators::default()).expect("parse");
        let settings = PreviewSettings {
            repeat_count: 30,
            ..PreviewSettings::default()
        };
        let frame = preview(&file, &settings, None).expect("preview");
        let shown: Vec<[f32; 4]> = frame
            .nodes
            .iter()
            .filter(|n| n.path == "/1/0" && n.visible)
            .map(|n| n.rect)
            .collect();

        let mut app = preview_app(&settings, None).expect("preview app");
        regia_ui::ViewModelAppExt::register_view_model::<FeedVm>(&mut app);
        let world = app.world_mut();
        world.resource_mut::<FeedVm>().items = (0..30)
            .map(|id| FeedRow {
                id,
                name: format!("{id}"),
            })
            .collect();
        world
            .resource_mut::<UiLibrary>()
            .templates
            .insert("game".to_owned(), file.clone());
        regia_ui::authoring::spawn_ui(world, "game").expect("spawn");
        for _ in 0..4 {
            app.update();
        }
        let world = app.world_mut();
        let mut rows = world.query::<(&NodeName, &ComputedNode, &ChildOf)>();
        let mut runtime: Vec<(usize, [f32; 4])> = rows
            .iter(world)
            .filter(|(n, ..)| n.0 == "row")
            .filter_map(|(_, c, parent)| {
                let index = world.get::<regia_ui::ListRow>(parent.0)?.index;
                Some((index, c.rect))
            })
            .collect();
        runtime.sort_by_key(|(i, _)| *i);
        assert_eq!(frame.lists[0].materialized, runtime.len(), "same row count");
        assert_eq!(shown.len(), runtime.len());
        for ((index, rect), preview_rect) in runtime.iter().zip(&shown) {
            let close = rect.iter().zip(preview_rect).all(|(a, b)| (a - b).abs() <= 1.0);
            assert!(close, "row {index}: {rect:?} vs {preview_rect:?}");
        }
    }
    #[test]
    fn project_dpi_curve_and_preview_rules_use_runtime_scale() {
        let root = regia_test_utils::unique_temp_dir("w12-preview-dpi");
        std::fs::create_dir_all(root.join("ui")).expect("valid test fixture");
        regia_test_utils::write_atomic(
            &root.join("ui/project.uisettings.ron"),
            b"(version:1,dpi_curve:[(96.0,1.0),(192.0,2.5)])",
        )
        .expect("valid test fixture");
        let file = regia_ui::authoring::UiFile {
            version: regia_ui::authoring::UI_RON_VERSION,
            root: Default::default(),
            ..Default::default()
        };
        let mut settings = PreviewSettings {
            assets_root: Some(root.clone()),
            scale_mode: Some(regia_ui::ScaleMode::ConstantPhysicalSize),
            dpi: 144.0,
            ..Default::default()
        };
        assert_eq!(
            preview(&file, &settings, None).expect("valid test fixture").scale,
            1.75
        );
        settings.scale = Some(1.5);
        assert_eq!(
            preview(&file, &settings, None).expect("valid test fixture").scale,
            1.5,
            "manual bypasses curve"
        );
        settings.scale = None;
        for mode in [
            regia_ui::ScaleMode::ConstantPixelSize,
            regia_ui::ScaleMode::default(),
            regia_ui::ScaleMode::IntegerScale {
                reference: [320.0, 180.0],
            },
        ] {
            settings.scale_mode = Some(mode);
            for size in [[1280.0, 720.0], [1920.0, 1080.0], [800.0, 1280.0]] {
                settings.size = size;
                assert_eq!(
                    preview(&file, &settings, None).expect("valid test fixture").scale,
                    mode.scale_for(size, 1.5)
                );
            }
        }
        std::fs::remove_dir_all(root).expect("valid test fixture");
    }

    #[test]
    fn the_saved_scale_rule_drives_the_preview_label() {
        let root = regia_test_utils::unique_temp_dir("w13-preview-scale-rule");
        std::fs::create_dir_all(root.join("ui")).expect("valid test fixture");
        regia_test_utils::write_atomic(
            &root.join("ui/project.uisettings.ron"),
            b"(version: 2, scale_mode: IntegerScale(reference: (320.0, 180.0)))",
        )
        .expect("valid test fixture");
        let file = regia_ui::authoring::UiFile {
            version: regia_ui::authoring::UI_RON_VERSION,
            root: Default::default(),
            ..Default::default()
        };
        let settings = PreviewSettings {
            assets_root: Some(root.clone()),
            size: [1280.0, 720.0],
            scale: None,
            scale_mode: None,
            ..Default::default()
        };
        assert_eq!(
            preview(&file, &settings, None).expect("valid test fixture").scale,
            4.0,
            "1280×720 at a saved 320×180 integer rule is scale 4"
        );
        std::fs::remove_dir_all(root).expect("valid test fixture");
    }
}
