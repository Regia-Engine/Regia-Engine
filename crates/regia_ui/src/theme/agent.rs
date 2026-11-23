// SPDX-License-Identifier: MPL-2.0
//! The theme agent surface, hot reload and preview (`docs/specs/ui-default-design.md` §§ 6.5,
//! 6.6; lane D7, ADR-330). Book page `book/agents/ui-theme.md`.
//!
//! - **Methods.** [`call`] answers `ui.theme_list`, `ui.theme_get`, `ui.theme_set`,
//!   `ui.theme_select`, `ui.theme_preview`, `ui.theme_lint`, `ui.theme_reload` and the
//!   `wait_until` predicate `ui.theme_generation`. The UI agent bridge
//!   ([`crate::agent_bridge::call`]) routes every `ui.theme_*` name here, so they travel the same
//!   host hook as the other `ui.*` methods (ADR-202, ADR-291). Writes change [`UiThemes`] or
//!   [`ActiveTheme`] only, so they land through D6's resolver on the next frame (one generation,
//!   cross-fade included).
//! - **Hot reload.** [`ThemeFiles`] remembers every `.theme.ron` read from disk. While the asset
//!   watcher runs (the editor, `regia run --dev`) or [`ThemeFiles::watch`] says so,
//!   [`watch_theme_files`] re-reads a file whose size or modification time changed (and any file
//!   named by a `SourceAssetChanged` message). A good file swaps in; a broken one keeps the last
//!   good theme, logs one warning with the key path and is reported by `ui.theme_reload` and
//!   `ui.theme_list`. Shipping builds run no watcher, so they read themes once.
//! - **Preview.** `ui.theme_preview` builds a separate headless UI app with the overrides, lays a
//!   page out for the requested size through the render-out path (ADR-202), draws it into an
//!   [`OffscreenTarget`] and returns the pixels. The live UI is never touched.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use serde_json::{Map, Value as Json, json};

use crate::agent_bridge::UiBridgeError;
use crate::color::UiColor;
use crate::node::{
    Classes, Disabled, Focused, Hovered, Invalid, NodeName, NodeStyle, Pressed, SafeAreaPadding, Selected,
    UiNode, UiRoot,
};
use crate::offscreen::OffscreenTarget;
use crate::render::SharedUiRenderer;
use crate::scale::{ScaleMode, UiScales};
use crate::style::{Align, Dimension, Edges, FlexDirection, JustifyContent, Style};
use crate::text::Text;
use crate::theme::{Themed, UiThemes};
use crate::theme_color_ops::to_hex;
use crate::theme_file::{
    BUILTIN_THEME_SOURCES, Density, PropMap, PropValue, Scheme, ThemeError, ThemeFile, ThemeSet, TokenValue,
    TypeDef, parse_theme_ron,
};
use crate::theme_lint::{LintFinding, ThemeLint, lint_file};
use crate::theme_resolve::{ActiveTheme, DEFAULT_THEME, ThemeResolver};
use crate::theme_tokens::{Easing, Value};
use crate::widget_style::{StyleState, WidgetSize};
use crate::widgets::{Button, ProgressBar, Slider, Toggle};

/// Every method name [`call`] answers.
pub const THEME_METHODS: [&str; 8] = [
    "ui.theme_list",
    "ui.theme_get",
    "ui.theme_set",
    "ui.theme_select",
    "ui.theme_preview",
    "ui.theme_lint",
    "ui.theme_reload",
    "ui.theme_generation",
];

/// The page `ui.theme_preview` renders when `page` is omitted ([`spawn_theme_sheet`]).
pub const THEME_SHEET_PAGE: &str = "theme-sheet";

/// Default preview size (UI px at scale 1).
pub const PREVIEW_DEFAULT_SIZE: [u32; 2] = [1280, 720];

/// Largest preview side, in pixels.
pub const PREVIEW_MAX_SIDE: u32 = 4096;

/// Frames the preview app runs before it is captured: themes resolve on the second, layout and
/// text settle on the third.
const PREVIEW_FRAMES: usize = 3;

/// Frames between two looks at the theme files on disk while watching.
pub const DEFAULT_POLL_FRAMES: u32 = 30;

// ---------------------------------------------------------------------------------------------
// Hot reload.
// ---------------------------------------------------------------------------------------------

/// When [`watch_theme_files`] looks at theme files on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeWatch {
    /// While the asset watcher runs (`regia_asset::AssetWatchStatus::watching`: the editor and
    /// `regia run --dev`). Shipping builds run no watcher, so they read themes once.
    #[default]
    Auto,
    /// Always (tests, tools).
    On,
    /// Never; `ui.theme_reload` still re-reads on request.
    Off,
}

/// Why a theme file did not load: the key path (`tokens.color.accent`, `extends`; empty for a
/// file that did not parse, whose message carries the line and column) and the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeLoadIssue {
    /// Key path of the bad value.
    pub key: String,
    /// What is wrong.
    pub message: String,
}

impl ThemeLoadIssue {
    /// The issue of a load error.
    pub fn of(error: &ThemeError) -> Self {
        let key = match error {
            ThemeError::Parse { .. } | ThemeError::UnknownTheme { .. } => String::new(),
            ThemeError::UnsupportedVersion { .. } => "format_version".to_owned(),
            ThemeError::Value { key, .. }
            | ThemeError::UnknownReference { key, .. }
            | ThemeError::Variation { key, .. } => key.clone(),
            // A property cycle reads `types.X.props.y: tokens.a -> ...`: the property is the key.
            ThemeError::Cycle { chain, .. } => match chain.split_once(": ") {
                Some((property, _)) => property.to_owned(),
                None => chain.split(" -> ").next().unwrap_or_default().to_owned(),
            },
            ThemeError::UnknownParent { .. } | ThemeError::ExtendsCycle { .. } => "extends".to_owned(),
        };
        Self {
            key,
            message: error.to_string(),
        }
    }

    fn json(&self) -> Json {
        json!({ "key": self.key, "message": self.message })
    }
}

/// Size and modification time of a file, to notice edits without reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

fn stamp_of(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(FileStamp {
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

/// One `.theme.ron` read from disk.
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeSource {
    /// Where the file is.
    pub path: PathBuf,
    /// The theme the file defines (after its first good load).
    pub theme: Option<String>,
    /// Why the last read failed; `None` after a good load.
    pub error: Option<ThemeLoadIssue>,
    /// Successful loads so far.
    pub loads: u32,
    /// The last version that loaded (what `ui.theme_set { reset: true }` goes back to).
    pub last_good: Option<ThemeFile>,
    stamp: Option<FileStamp>,
}

/// The theme files read from disk and how they are watched (spec § 6.5).
#[derive(Resource, Debug)]
pub struct ThemeFiles {
    /// When to watch the files.
    pub watch: ThemeWatch,
    /// Frames between two looks at the files while watching (minimum 1).
    pub poll_frames: u32,
    sources: Vec<ThemeSource>,
    pending: Vec<PathBuf>,
    frame: u32,
    root: Option<PathBuf>,
}

impl Default for ThemeFiles {
    fn default() -> Self {
        Self {
            watch: ThemeWatch::Auto,
            poll_frames: DEFAULT_POLL_FRAMES,
            sources: Vec::new(),
            pending: Vec::new(),
            frame: 0,
            root: None,
        }
    }
}

impl ThemeFiles {
    /// Every known theme file.
    pub fn sources(&self) -> &[ThemeSource] {
        &self.sources
    }

    /// The file a theme was loaded from.
    pub fn path_of(&self, theme: &str) -> Option<&Path> {
        self.source_of(theme).map(|s| s.path.as_path())
    }

    /// How answers and logs show `path`: `assets/...` relative to the project root when the app
    /// has an asset server (no absolute host paths in answers, ADR-340), else as given.
    pub fn shown(&self, path: &Path) -> String {
        match &self.root {
            Some(root) => regia_asset::display_path(root, path),
            None => path.display().to_string(),
        }
    }

    fn source_of(&self, theme: &str) -> Option<&ThemeSource> {
        self.sources.iter().find(|s| s.theme.as_deref() == Some(theme))
    }

    /// Queues a file for re-reading on the next frame (what a `SourceAssetChanged` message does).
    pub fn mark_changed(&mut self, path: impl Into<PathBuf>) {
        self.pending.push(path.into());
    }

    fn index_of(&mut self, path: &Path) -> usize {
        if let Some(i) = self.sources.iter().position(|s| s.path == path) {
            return i;
        }
        self.sources.push(ThemeSource {
            path: path.to_owned(),
            theme: None,
            error: None,
            loads: 0,
            last_good: None,
            stamp: None,
        });
        self.sources.len() - 1
    }
}

/// Reads and checks one theme file against `set`; returns the theme name, its file and the set
/// with it inserted. Touches nothing.
fn read_theme(path: &Path, shown: &str, set: &ThemeSet) -> Result<(String, ThemeFile, ThemeSet), ThemeError> {
    let text = std::fs::read_to_string(path).map_err(|e| ThemeError::Parse {
        path: shown.to_owned(),
        message: e.to_string(),
    })?;
    let file = parse_theme_ron(&text, shown)?;
    let mut next = set.clone();
    next.insert(file.clone())?;
    Ok((file.name.clone(), file, next))
}

/// What re-reading one file did.
#[derive(Debug, Clone, PartialEq)]
enum ReloadOutcome {
    Loaded { theme: String, changed: bool },
    Failed(ThemeLoadIssue),
}

/// Re-reads source `index` against `set`. A good file returns the set with its theme replaced,
/// only when that differs (so an untouched save costs no generation); a bad one keeps the last
/// good theme and logs one warning with the key path. The caller writes the returned set.
fn reload_source(
    files: &mut ThemeFiles,
    index: usize,
    set: &ThemeSet,
    status: Option<&mut regia_asset::AssetWatchStatus>,
) -> (ReloadOutcome, Option<ThemeSet>) {
    let shown = files.shown(&files.sources[index].path);
    let source = &mut files.sources[index];
    source.stamp = stamp_of(&source.path);
    let mut replacement = None;
    let outcome = match read_theme(&source.path, &shown, set) {
        Ok((theme, file, next)) => {
            let changed = next != *set;
            if changed {
                replacement = Some(next);
            }
            source.theme = Some(theme.clone());
            source.error = None;
            source.loads += 1;
            source.last_good = Some(file);
            tracing::info!(target: "regia_ui", "theme `{theme}` loaded from {shown}");
            ReloadOutcome::Loaded { theme, changed }
        }
        Err(error) => {
            let issue = ThemeLoadIssue::of(&error);
            tracing::warn!(
                target: "regia_ui",
                "theme file {shown}: {} (key `{}`); keeping the last good theme",
                issue.message,
                issue.key
            );
            source.error = Some(issue.clone());
            ReloadOutcome::Failed(issue)
        }
    };
    if let Some(status) = status {
        status.record(regia_asset::WatchEntry {
            path: shown,
            kind: regia_asset::SourceKind::Other,
            millis: 0,
            outcome: match &outcome {
                ReloadOutcome::Loaded { .. } => regia_asset::WatchOutcome::Reloaded,
                ReloadOutcome::Failed(issue) => regia_asset::WatchOutcome::Error(issue.message.clone()),
            },
        });
    }
    (outcome, replacement)
}

/// Re-reads the given sources into the world's [`UiThemes`], in order (a parent reloaded first is
/// what its children see). `UiThemes` is written only when a theme changed.
fn reload_in_world(world: &mut World, indices: &[usize]) -> Vec<(usize, ReloadOutcome)> {
    world.init_resource::<ThemeFiles>();
    world.resource_scope(|world, mut files: Mut<ThemeFiles>| {
        let mut status = world.remove_resource::<regia_asset::AssetWatchStatus>();
        let mut out = Vec::with_capacity(indices.len());
        for &i in indices {
            let (outcome, replacement) = {
                let set = &world.resource::<UiThemes>().set;
                reload_source(&mut files, i, set, status.as_mut())
            };
            if let Some(next) = replacement {
                world.resource_mut::<UiThemes>().set = next;
            }
            out.push((i, outcome));
        }
        if let Some(status) = status {
            world.insert_resource(status);
        }
        out
    })
}

/// Loads a `.theme.ron` from disk into [`UiThemes`] and remembers it for hot reload and
/// `persist`. Returns the theme name. A file that fails stays registered (it is retried when it
/// changes) and the error is returned.
pub fn load_theme_file(world: &mut World, path: &Path) -> Result<String, ThemeLoadIssue> {
    world.init_resource::<ThemeFiles>();
    let root = world
        .get_resource::<regia_asset::AssetServer>()
        .map(|server| server.project_root().to_path_buf());
    let index = {
        let mut files = world.resource_mut::<ThemeFiles>();
        if root.is_some() {
            files.root = root;
        }
        files.index_of(path)
    };
    match reload_in_world(world, &[index]).pop().map(|(_, o)| o) {
        Some(ReloadOutcome::Loaded { theme, .. }) => Ok(theme),
        Some(ReloadOutcome::Failed(issue)) => Err(issue),
        None => Err(ThemeLoadIssue {
            key: String::new(),
            message: format!("{} was not read", path.display()),
        }),
    }
}

/// `Startup` (when [`crate::UiPlugin::load_project_assets`] is on): loads every
/// `assets://ui/themes/*.theme.ron`, parents before children. Files on the native file system
/// are remembered for hot reload; files in a pak are read once.
pub fn load_project_themes(world: &mut World) {
    let Some(vfs) = world.get_resource::<regia_platform::Vfs>().cloned() else {
        return;
    };
    let Ok(dir) = regia_platform::VfsPath::parse("assets://ui/themes") else {
        return;
    };
    let Ok(entries) = vfs.list_dir(&dir) else {
        return;
    };
    let mut todo: Vec<regia_platform::VfsPath> = entries
        .iter()
        .filter(|e| !e.is_dir && e.name.to_ascii_lowercase().ends_with(".theme.ron"))
        .filter_map(|e| regia_platform::VfsPath::parse(&format!("assets://ui/themes/{}", e.name)).ok())
        .collect();
    // A child may be listed before its parent: retry until a pass loads nothing new.
    let last: Vec<(String, ThemeLoadIssue)> = loop {
        let mut failed = Vec::new();
        let mut progressed = false;
        for path in std::mem::take(&mut todo) {
            let result = match vfs.host_path(&path) {
                Some(native) => load_theme_file(world, &native),
                None => load_packed_theme(world, &vfs, &path),
            };
            match result {
                Ok(_) => progressed = true,
                Err(issue) => failed.push((path, issue)),
            }
        }
        if failed.is_empty() || !progressed {
            break failed.into_iter().map(|(p, i)| (p.to_string(), i)).collect();
        }
        todo = failed.into_iter().map(|(p, _)| p).collect();
    };
    for (path, issue) in last {
        tracing::warn!(target: "regia_ui", "theme file {path}: {} (key `{}`)", issue.message, issue.key);
    }
}

/// Reads a theme that has no native path (a pak entry): loaded once, never watched.
fn load_packed_theme(
    world: &mut World,
    vfs: &regia_platform::Vfs,
    path: &regia_platform::VfsPath,
) -> Result<String, ThemeLoadIssue> {
    let bytes = vfs.read_sync(path).map_err(|e| ThemeLoadIssue {
        key: String::new(),
        message: e.to_string(),
    })?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let file = parse_theme_ron(&text, &path.to_string()).map_err(|e| ThemeLoadIssue::of(&e))?;
    let name = file.name.clone();
    let mut next = world.resource::<UiThemes>().set.clone();
    next.insert(file).map_err(|e| ThemeLoadIssue::of(&e))?;
    world.resource_mut::<UiThemes>().set = next;
    Ok(name)
}

/// Whether theme files are watched now.
fn watching(files: &ThemeFiles, status: Option<&regia_asset::AssetWatchStatus>) -> bool {
    match files.watch {
        ThemeWatch::On => true,
        ThemeWatch::Off => false,
        ThemeWatch::Auto => status.is_some_and(|s| s.watching),
    }
}

/// `PreUpdate`, before `apply_themes`: re-reads changed theme files (spec § 6.5). Idle cost is a
/// frame counter; while watching it looks at each file's size and modification time every
/// [`ThemeFiles::poll_frames`] frames, and reads only files that changed.
pub fn watch_theme_files(
    mut files: ResMut<ThemeFiles>,
    mut themes: ResMut<UiThemes>,
    mut status: Option<ResMut<regia_asset::AssetWatchStatus>>,
    mut changes: MessageReader<regia_asset::SourceAssetChanged>,
) {
    let files = &mut *files;
    for change in changes.read() {
        let is_theme = change
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.to_ascii_lowercase().ends_with(".theme.ron"));
        if is_theme {
            files.pending.push(change.path.clone());
        }
    }
    let watch = watching(files, status.as_deref());
    files.frame = files.frame.wrapping_add(1);
    let poll = watch && !files.sources.is_empty() && files.frame.is_multiple_of(files.poll_frames.max(1));
    if files.pending.is_empty() && !poll {
        return;
    }
    let _span = tracing::trace_span!("ui_theme_watch").entered();
    let mut todo: Vec<usize> = Vec::new();
    for path in std::mem::take(&mut files.pending) {
        // Only files a theme was read from are reloaded; a new file needs `load_theme_file`.
        if let Some(i) = files.sources.iter().position(|s| s.path == path) {
            todo.push(i);
        }
    }
    if poll {
        for (i, source) in files.sources.iter().enumerate() {
            if stamp_of(&source.path) != source.stamp {
                todo.push(i);
            }
        }
    }
    todo.sort_unstable();
    todo.dedup();
    for i in todo {
        // `themes` is read through `Deref` and written only when a theme changed, so an
        // unchanged file does not bump the theme generation.
        let (_, replacement) = reload_source(files, i, &themes.set, status.as_deref_mut());
        if let Some(next) = replacement {
            themes.set = next;
        }
    }
}

/// Installs [`ThemeFiles`], the watcher and (optionally) the project theme loader.
/// [`crate::UiPlugin`] adds it.
pub struct ThemeAgentPlugin {
    /// Load `assets://ui/themes/*.theme.ron` at startup.
    pub load_project_themes: bool,
}

impl Plugin for ThemeAgentPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ThemeFiles>()
            .add_systems(PreUpdate, watch_theme_files.before(crate::theme::apply_themes));
        if self.load_project_themes {
            app.add_systems(Startup, load_project_themes);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Methods.
// ---------------------------------------------------------------------------------------------

fn error(
    class: &'static str,
    reason: &'static str,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> UiBridgeError {
    UiBridgeError {
        class,
        reason,
        message: message.into(),
        hint: hint.into(),
    }
}

fn bad_param(message: impl Into<String>, hint: impl Into<String>) -> UiBridgeError {
    error("invalid_params", "ui_bad_param", message, hint)
}

fn unknown_theme(name: &str, set: &ThemeSet) -> UiBridgeError {
    let names: Vec<&str> = set.names().collect();
    error(
        "not_found",
        "ui_theme_unknown",
        format!("no theme named `{name}` is loaded"),
        format!("loaded themes: {}", names.join(", ")),
    )
}

fn invalid_theme(e: &ThemeError) -> UiBridgeError {
    let issue = ThemeLoadIssue::of(e);
    let message = if issue.key.is_empty() {
        issue.message
    } else {
        format!("{}: {}", issue.key, issue.message)
    };
    error(
        "invalid_params",
        "ui_theme_invalid",
        message,
        "nothing was applied; fix the value at the key path and call again",
    )
}

/// True when [`call`] answers `method`.
pub fn handles(method: &str) -> bool {
    THEME_METHODS.contains(&method)
}

/// Runs one theme method (see the module docs and `book/agents/ui-theme.md`).
pub fn call(world: &mut World, method: &str, params: &Json) -> Result<Json, UiBridgeError> {
    let _span = tracing::trace_span!("ui_theme_call").entered();
    if world.get_resource::<UiThemes>().is_none() {
        return Err(error(
            "unsupported",
            "ui_not_installed",
            "this app has no UiThemes resource",
            "add regia_ui::UiPlugin",
        ));
    }
    world.init_resource::<ThemeFiles>();
    world.init_resource::<ActiveTheme>();
    match method {
        "ui.theme_list" => Ok(theme_list(world)),
        "ui.theme_get" => theme_get(world, params),
        "ui.theme_set" => theme_set(world, params),
        "ui.theme_select" => theme_select(world, params),
        "ui.theme_preview" => theme_preview(world, params),
        "ui.theme_lint" => theme_lint(world, params),
        "ui.theme_reload" => theme_reload(world, params),
        "ui.theme_generation" => theme_generation(world, params),
        other => Err(error(
            "not_found",
            "ui_unknown_method",
            format!("`{other}` is not a ui theme method"),
            THEME_METHODS.join(", "),
        )),
    }
}

fn str_param(params: &Json, key: &str) -> Result<Option<String>, UiBridgeError> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(Json::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(bad_param(
            format!("`{key}` must be a string"),
            "see book/agents/ui-theme.md",
        )),
    }
}

fn bool_param(params: &Json, key: &str) -> Result<Option<bool>, UiBridgeError> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(Json::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(bad_param(
            format!("`{key}` must be true or false"),
            "see book/agents/ui-theme.md",
        )),
    }
}

/// The density a call that omits `density` resolves with: the active override, then
/// [`crate::widget_style::UiDensity`]. `None` means "the requested theme's own density".
fn active_density(world: &World) -> Option<Density> {
    if let Some(density) = world
        .get_resource::<ActiveTheme>()
        .and_then(|theme| theme.density)
    {
        return Some(density);
    }
    world
        .get_resource::<crate::widget_style::UiDensity>()
        .copied()
        .map(|density| match density {
            crate::widget_style::UiDensity::Comfortable => Density::Comfortable,
            crate::widget_style::UiDensity::Compact => Density::Compact,
        })
}

fn density_param(params: &Json) -> Result<Option<Density>, UiBridgeError> {
    match str_param(params, "density")?.as_deref() {
        None => Ok(None),
        Some("comfortable") => Ok(Some(Density::Comfortable)),
        Some("compact") => Ok(Some(Density::Compact)),
        Some(other) => Err(bad_param(
            format!("`density` must be \"comfortable\" or \"compact\", not `{other}`"),
            "omit it to keep the current density",
        )),
    }
}

fn scheme_param(params: &Json) -> Result<Option<Scheme>, UiBridgeError> {
    match str_param(params, "scheme")?.as_deref() {
        None => Ok(None),
        Some("dark") => Ok(Some(Scheme::Dark)),
        Some("light") => Ok(Some(Scheme::Light)),
        Some(other) => Err(bad_param(
            format!("`scheme` must be \"dark\" or \"light\", not `{other}`"),
            "omit it to keep the theme's own scheme",
        )),
    }
}

fn scheme_name(s: Scheme) -> &'static str {
    match s {
        Scheme::Dark => "dark",
        Scheme::Light => "light",
    }
}

fn generation(world: &World) -> u64 {
    world
        .get_resource::<ThemeResolver>()
        .map_or(0, ThemeResolver::generation)
}

/// The theme a call without `theme` means: the one nodes resolve against.
fn default_theme(world: &World) -> String {
    world
        .get_resource::<ThemeResolver>()
        .map(|r| r.active().to_owned())
        .filter(|n| !n.is_empty())
        .or_else(|| world.get_resource::<ActiveTheme>().map(|a| a.name.clone()))
        .unwrap_or_else(|| DEFAULT_THEME.to_owned())
}

/// The `theme` parameter (or the default), checked to be loaded.
fn theme_param(world: &World, params: &Json) -> Result<String, UiBridgeError> {
    let name = str_param(params, "theme")?.unwrap_or_else(|| default_theme(world));
    let set = &world.resource::<UiThemes>().set;
    if set.get(&name).is_none() {
        return Err(unknown_theme(&name, set));
    }
    Ok(name)
}

fn chain_scheme(set: &ThemeSet, name: &str) -> Scheme {
    set.chain(name)
        .ok()
        .and_then(|c| c.iter().find_map(|f| f.scheme))
        .unwrap_or_default()
}

fn chain_density(set: &ThemeSet, name: &str) -> Density {
    set.chain(name)
        .ok()
        .and_then(|c| c.iter().find_map(|f| f.density))
        .unwrap_or_default()
}

/// Rounds for display: themes are authored with a few decimals, f32 noise is not useful.
fn num(n: f32) -> Json {
    let rounded = (f64::from(n) * 10_000.0).round() / 10_000.0;
    json!(rounded)
}

fn duration_text(seconds: f32) -> String {
    let ms = (f64::from(seconds) * 1000.0 * 1000.0).round() / 1000.0;
    format!("{ms}ms")
}

/// A resolved token value as JSON: colours as hex, durations as `"80ms"`, easings as written.
fn value_json(v: &Value) -> Json {
    match v {
        Value::Color(c) => json!(to_hex(*c)),
        Value::Number(n) => num(*n),
        Value::Duration(d) => json!(duration_text(*d)),
        Value::Easing(Easing::Linear) => json!("linear"),
        Value::Easing(Easing::CubicBezier([a, b, c, d])) => {
            json!(format!("cubic-bezier({a}, {b}, {c}, {d})"))
        }
        Value::Text(t) => json!(t),
    }
}

fn color_json(c: Option<UiColor>) -> Json {
    c.map_or(Json::Null, |c| json!(to_hex(c)))
}

fn opt_num(n: Option<f32>) -> Json {
    n.map_or(Json::Null, num)
}

fn token_value_json(v: &TokenValue) -> Json {
    match v {
        TokenValue::Number(n) => num(*n),
        TokenValue::Text(t) => json!(t),
    }
}

fn finding_json(f: &LintFinding) -> Json {
    let mut out = Map::new();
    out.insert("kind".into(), json!(f.kind.name()));
    out.insert("key".into(), json!(f.key));
    out.insert("message".into(), json!(f.message));
    if let Some(pair) = &f.pair {
        out.insert("pair".into(), json!(pair));
    }
    if let Some(ratio) = f.ratio {
        out.insert("ratio".into(), num(ratio));
    }
    if let Some(required) = f.required {
        out.insert("required".into(), num(required));
    }
    if let Some(value) = f.value {
        out.insert("value".into(), num(value));
    }
    Json::Object(out)
}

fn lint_json(lint: &ThemeLint) -> Json {
    json!({
        "theme": lint.theme,
        "scheme": scheme_name(lint.scheme),
        "density": lint.density.key(),
        "clean": lint.is_clean(),
        "findings": lint.findings.iter().map(finding_json).collect::<Vec<_>>(),
    })
}

/// `*` matches any run of characters, `?` one character; everything else matches itself.
fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

fn keys_param(params: &Json) -> Result<Vec<String>, UiBridgeError> {
    match params.get("keys").filter(|v| !v.is_null()) {
        None => Ok(Vec::new()),
        Some(Json::String(s)) => Ok(vec![s.clone()]),
        Some(Json::Array(list)) => list
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                bad_param(
                    "`keys` entries must be strings",
                    "e.g. [\"color.*\", \"radius.md\"]",
                )
            }),
        Some(_) => Err(bad_param(
            "`keys` must be a glob or a list of globs",
            "e.g. \"color.*\"",
        )),
    }
}

/// `ui.theme_list`.
fn theme_list(world: &mut World) -> Json {
    let themes = world.resource::<UiThemes>();
    let files = world.resource::<ThemeFiles>();
    let active = world.resource::<ActiveTheme>();
    let resolver = world.get_resource::<ThemeResolver>();
    let builtin: Vec<&str> = BUILTIN_THEME_SOURCES.iter().map(|(n, _)| *n).collect();
    let list: Vec<Json> = themes
        .set
        .names()
        .map(|name| {
            let file = themes.set.get(name);
            let source = files.source_of(name);
            json!({
                "name": name,
                "extends": file.and_then(|f| f.extends.clone()),
                "scheme": scheme_name(chain_scheme(&themes.set, name)),
                "density": chain_density(&themes.set, name).key(),
                "path": source.map(|s| files.shown(&s.path)),
                "builtin": builtin.contains(&name) && source.is_none(),
                "error": source.and_then(|s| s.error.as_ref()).map(ThemeLoadIssue::json),
            })
        })
        .collect();
    let sources: Vec<Json> = files
        .sources()
        .iter()
        .map(|s| {
            json!({
                "path": files.shown(&s.path),
                "theme": s.theme,
                "loads": s.loads,
                "error": s.error.as_ref().map(ThemeLoadIssue::json),
            })
        })
        .collect();
    let resolved = resolver.and_then(ThemeResolver::resolved_theme);
    let density = resolved.map(|t| t.density).or(active.density).unwrap_or_default();
    // The named token slots widgets read (ADR-7208), with the resolved theme's value (null when
    // it leaves the slot to the engine fallback).
    let slots: Vec<Json> = crate::theme_tokens::TOKEN_SLOTS
        .iter()
        .map(|s| {
            json!({
                "key": s.key,
                "kind": s.kind,
                "used_by": s.used_by,
                "fallback": s.fallback,
                "value": resolved.and_then(|t| t.values.get(s.key)).map(value_json),
            })
        })
        .collect();
    json!({
        "slots": slots,
        "themes": list,
        "active": active.name,
        "resolved": resolver.map(|r| r.active().to_owned()).filter(|n| !n.is_empty()),
        "density": density.key(),
        "high_contrast": active.high_contrast,
        "reduced_motion": active.reduced_motion,
        "generation": resolver.map_or(0, ThemeResolver::generation),
        "error": resolver.and_then(ThemeResolver::error),
        "files": sources,
        "watching": watching(files, world.get_resource::<regia_asset::AssetWatchStatus>()),
    })
}

/// The type/variation keys a class resolves through, root first (`Button`, `Button.primary`,
/// `Button.danger`): the variation's `extends`, else the part before its last dot.
fn type_chain(theme: &crate::theme_file::ResolvedTheme, key: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = Some(key.to_owned());
    while let Some(k) = current.take() {
        if out.contains(&k) {
            break;
        }
        if let Some(def) = theme.variations.get(&k) {
            current = def
                .extends
                .clone()
                .or_else(|| k.rsplit_once('.').map(|(b, _)| b.to_owned()));
            out.push(k);
        } else if theme.types.contains_key(&k) {
            out.push(k);
        }
    }
    out.reverse();
    out
}

fn state_param(params: &Json) -> Result<StyleState, UiBridgeError> {
    let names: Vec<String> = match params.get("state").filter(|v| !v.is_null()) {
        None => Vec::new(),
        Some(Json::String(s)) => vec![s.clone()],
        Some(Json::Array(list)) => list
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                bad_param(
                    "`state` entries must be strings",
                    "e.g. [\"hovered\", \"focused\"]",
                )
            })?,
        Some(_) => {
            return Err(bad_param(
                "`state` must be a state name or a list of them",
                "normal, hovered, pressed, focused, disabled, selected, error",
            ));
        }
    };
    let mut state = StyleState::default();
    for name in names {
        match name.to_ascii_lowercase().as_str() {
            "normal" => {}
            "hovered" | "hover" => state.hovered = true,
            "pressed" => state.pressed = true,
            "focused" => state.focused = true,
            "disabled" => state.disabled = true,
            "selected" => state.selected = true,
            "error" | "invalid" => state.error = true,
            "touch" => state.touch = true,
            other => {
                return Err(error(
                    "invalid_params",
                    "ui_unknown_state",
                    format!("`{other}` is not a widget state"),
                    "normal, hovered, pressed, focused, disabled, selected, error, touch",
                ));
            }
        }
    }
    state.size = match str_param(params, "size")?.as_deref() {
        None | Some("md") => WidgetSize::Md,
        Some("sm") => WidgetSize::Sm,
        Some("lg") => WidgetSize::Lg,
        Some(other) => {
            return Err(bad_param(
                format!("`size` must be sm, md or lg, not `{other}`"),
                "omit it for md",
            ));
        }
    };
    Ok(state)
}

fn state_names(state: &StyleState) -> Vec<&'static str> {
    let flags = [
        (state.hovered, "hovered"),
        (state.pressed, "pressed"),
        (state.focused, "focused"),
        (state.disabled, "disabled"),
        (state.selected, "selected"),
        (state.error, "error"),
        (state.touch, "touch"),
    ];
    flags.iter().filter(|(on, _)| *on).map(|(_, n)| *n).collect()
}

/// Resolves one class in one state through D6's resolver (`apply_themes`), in a scratch world:
/// exactly what a live node with that class and state would show.
pub fn resolve_widget_look(
    themes: &UiThemes,
    active: &ActiveTheme,
    class: &str,
    state: &StyleState,
) -> Result<Themed, String> {
    use bevy_ecs::system::RunSystemOnce;
    let mut world = World::new();
    world.insert_resource(themes.clone());
    // Reduced motion: the look is final at once. It also keeps the selection from equalling the
    // default, which would let a legacy palette index chosen before the first frame override it.
    let mut active = active.clone();
    active.reduced_motion = true;
    world.insert_resource(active);
    world.init_resource::<ThemeResolver>();
    world.init_resource::<Time>();
    let node = {
        let mut e = world.spawn((UiNode, Classes(vec![class.to_owned()]), state.size));
        if state.hovered {
            e.insert(Hovered);
        }
        if state.pressed {
            e.insert(Pressed);
        }
        if state.focused {
            e.insert(Focused);
        }
        if state.disabled {
            e.insert(Disabled);
        }
        if state.selected {
            e.insert(Selected);
        }
        if state.error {
            e.insert(Invalid);
        }
        e.id()
    };
    // Touch is a pointer fact, not a marker: the live resolver skips hover only while
    // `UiPointer::active_touch` is set (`apply_themes`). The scratch world has to say the same.
    if state.touch {
        world.insert_resource(crate::interaction::UiPointer {
            active_touch: Some(1),
            ..Default::default()
        });
    }
    // The first run gives the node its `Themed`, the second resolves it.
    for _ in 0..2 {
        world
            .run_system_once(crate::theme::apply_themes)
            .map_err(|e| e.to_string())?;
    }
    world
        .get::<Themed>(node)
        .copied()
        .ok_or_else(|| "the resolver did not theme the node".to_owned())
}

fn themed_json(t: &Themed) -> Json {
    json!({
        "background": color_json(t.background),
        "border_color": color_json(t.border_color),
        "border_width": opt_num(t.border_width),
        "radius": opt_num(t.radius),
        "text_color": color_json(t.text_color),
        "focus_ring": color_json(t.focus_ring),
        "focus_ring_width": opt_num(t.focus_ring_width),
        "focus_ring_offset": opt_num(t.focus_ring_offset),
        "opacity": opt_num(t.opacity),
        "height": opt_num(t.height),
        "padding_x": opt_num(t.padding_x),
        "min_width": opt_num(t.min_width),
        "padding": t.padding.map(|[l, top, r, b]| {
            json!({"left": num(l), "top": num(top), "right": num(r), "bottom": num(b)})
        }),
        "min_height": opt_num(t.min_height),
        "max_width": opt_num(t.max_width),
        "max_height": opt_num(t.max_height),
        "gap": opt_num(t.gap),
        // What the theme moves in layout (ADR-9340): its own height and inline padding (not the
        // `WidgetSize` token values above) and the gap.
        "layout": {
            "height": opt_num(t.layout_height),
            "padding_x": opt_num(t.layout_padding_x),
            "gap": opt_num(t.gap),
        },
        "text_px": opt_num(t.text_px),
        "icon_color": color_json(t.icon_color),
        "elevation": t.elevation,
        "translate": t.translate.map(|[x, y]| json!([num(x), num(y)])),
        "scale": opt_num(t.scale),
    })
}

/// The caption `subtitle` style of a resolved theme (spec 36 § 8.3, ADR-7208, ADR-9340), as the
/// caption rows read it: `null` where the theme leaves a slot unset.
fn caption_json(theme: &crate::theme_file::ResolvedTheme) -> Json {
    let style = crate::captions::CaptionThemeStyle::of_theme(theme);
    json!({
        "text": color_json(style.text),
        "plate": color_json(style.plate),
        "plate_opacity": opt_num(style.plate_opacity),
        "font": style.font,
        "size": opt_num(style.size),
        "outline": color_json(style.outline),
        "outline_width": opt_num(style.outline_width),
    })
}

/// `ui.theme_get`.
fn theme_get(world: &mut World, params: &Json) -> Result<Json, UiBridgeError> {
    let name = theme_param(world, params)?;
    let globs = keys_param(params)?;
    let with_values = bool_param(params, "resolved")?.unwrap_or(true);
    let ty = str_param(params, "type")?;
    let state = state_param(params)?;
    // Omitted `density` is the active density for every theme name: `ActiveTheme.density`,
    // else `UiDensity`. `None` lets the requested file keep its declared density. The resolved
    // theme's name is not a filter (high contrast resolves a different name than the request).
    let density = density_param(params)?.or(active_density(world));
    let themes = world.resource::<UiThemes>();
    let resolved = themes
        .set
        .resolve(&name, density)
        .map_err(|e| invalid_theme(&e))?;
    let chain = themes.set.chain(&name).map_err(|e| invalid_theme(&e))?;
    let Some(file) = themes.set.get(&name) else {
        return Err(unknown_theme(&name, &themes.set));
    };

    // Raw expressions, nearest definition first (a density overlay beats the base table).
    let mut tokens = Map::new();
    for key in resolved.values.keys() {
        if !globs.is_empty() && !globs.iter().any(|g| glob_match(g, key)) {
            continue;
        }
        let found = chain.iter().find_map(|f| {
            f.densities
                .get(resolved.density.key())
                .and_then(|d| d.get(key))
                .map(|v| (f.name.as_str(), v, true))
                .or_else(|| f.tokens.get(key).map(|v| (f.name.as_str(), v, false)))
        });
        let mut entry = Map::new();
        if let Some((from, raw, overlay)) = found {
            entry.insert("raw".into(), token_value_json(raw));
            entry.insert("from".into(), json!(from));
            if overlay {
                entry.insert("density".into(), json!(resolved.density.key()));
            }
        }
        if with_values && let Some(v) = resolved.values.get(key) {
            entry.insert("value".into(), value_json(v));
        }
        tokens.insert(key.clone(), Json::Object(entry));
    }

    let mut out = Map::new();
    out.insert("name".into(), json!(name));
    out.insert("extends".into(), json!(file.extends));
    out.insert("format_version".into(), json!(file.format_version));
    out.insert("scheme".into(), json!(scheme_name(resolved.scheme)));
    out.insert("density".into(), json!(resolved.density.key()));
    out.insert("chain".into(), json!(resolved.chain));
    out.insert("tokens".into(), Json::Object(tokens));
    out.insert("caption".into(), caption_json(&resolved));
    // `.theme.ron` v2 style rules of the chain, root theme first: the selector as display text
    // (never parsed back), its specificity and the raw properties.
    let rules: Vec<Json> = resolved
        .rules
        .iter()
        .map(|r| {
            let (a, b) = r.specificity();
            json!({
                "selector": r.display(),
                "specificity": [a, b],
                "state": r.state.map(crate::theme_file::RuleState::name),
                "props": serde_json::to_value(&r.props).unwrap_or(Json::Null),
            })
        })
        .collect();
    out.insert("rules".into(), Json::Array(rules));
    if let Some(ty) = &ty {
        let keys = type_chain(&resolved, ty);
        if keys.is_empty() {
            return Err(error(
                "not_found",
                "ui_theme_type_unknown",
                format!("theme `{name}` defines no widget type or variation `{ty}`"),
                "ui.theme_get without `type` lists the theme; types live under `types` and `variations` in the file",
            ));
        }
        let mut types = Map::new();
        for k in &keys {
            let def = resolved.variations.get(k).or_else(|| resolved.types.get(k));
            types.insert(
                k.clone(),
                def.and_then(|d| serde_json::to_value(d).ok())
                    .unwrap_or(Json::Null),
            );
        }
        out.insert("types".into(), Json::Object(types));
        if with_values {
            let mut active = world.resource::<ActiveTheme>().clone();
            active.name = name.clone();
            active.high_contrast = false;
            active.density = density;
            let themes = world.resource::<UiThemes>();
            let look = resolve_widget_look(themes, &active, ty, &state)
                .map_err(|m| error("rejected", "ui_theme_resolve_failed", m, "report this as a bug"))?;
            let size = match state.size {
                WidgetSize::Sm => "sm",
                WidgetSize::Md => "md",
                WidgetSize::Lg => "lg",
            };
            out.insert(
                "resolved".into(),
                json!({
                    "type": ty,
                    "chain": keys,
                    "state": state_names(&state),
                    "size": size,
                    "props": themed_json(&look),
                }),
            );
        }
    }
    Ok(Json::Object(out))
}

/// JSON (number, string, list, null) to a token value; `Ok(None)` removes the key.
fn token_from_json(key: &str, v: &Json) -> Result<Option<TokenValue>, UiBridgeError> {
    match v {
        Json::Null => Ok(None),
        Json::Number(n) => n
            .as_f64()
            .filter(|f| f.is_finite())
            .map(|f| Some(TokenValue::Number(f as f32)))
            .ok_or_else(|| {
                bad_param(
                    format!("tokens.{key}: not a finite number"),
                    "use a number or a string",
                )
            }),
        Json::String(s) => Ok(Some(TokenValue::Text(s.clone()))),
        _ => Err(bad_param(
            format!("tokens.{key}: a token is a number or a string (null removes it)"),
            "e.g. { \"color.accent\": \"#e8793a\", \"radius.md\": 4 }",
        )),
    }
}

fn prop_from_json(path: &str, v: &Json) -> Result<PropValue, UiBridgeError> {
    match v {
        Json::Number(n) => n
            .as_f64()
            .filter(|f| f.is_finite())
            .map(|f| PropValue::Number(f as f32))
            .ok_or_else(|| bad_param(format!("{path}: not a finite number"), "use a number")),
        Json::String(s) => Ok(PropValue::Text(s.clone())),
        Json::Array(items) => items
            .iter()
            .map(|i| prop_from_json(path, i))
            .collect::<Result<Vec<_>, _>>()
            .map(PropValue::List),
        _ => Err(bad_param(
            format!("{path}: a property is a number, a string or a list of those (null removes it)"),
            "e.g. { \"radius\": 4, \"border\": [1, \"@color.border\"] }",
        )),
    }
}

fn edit_props(path: &str, into: &mut PropMap, edits: &Json) -> Result<(), UiBridgeError> {
    let Some(map) = edits.as_object() else {
        return Err(bad_param(
            format!("{path} must be an object of properties"),
            "e.g. { \"radius\": 4 }",
        ));
    };
    for (prop, v) in map {
        if v.is_null() {
            into.remove(prop);
        } else {
            into.insert(prop.clone(), prop_from_json(&format!("{path}.{prop}"), v)?);
        }
    }
    Ok(())
}

/// Applies a `types` / `variations` edit: `null` removes a definition; an object merges its
/// `extends` and state layers key by key (`null` removes a property).
fn edit_defs(
    group: &str,
    defs: &mut std::collections::BTreeMap<String, TypeDef>,
    edits: &Json,
) -> Result<(), UiBridgeError> {
    let Some(map) = edits.as_object() else {
        return Err(bad_param(
            format!("`{group}` must be an object keyed by type name"),
            "e.g. { \"Button\": { \"props\": { \"radius\": 2 } } }",
        ));
    };
    for (name, edit) in map {
        if edit.is_null() {
            defs.remove(name);
            continue;
        }
        let Some(fields) = edit.as_object() else {
            return Err(bad_param(
                format!("{group}.{name} must be an object or null"),
                "fields: extends, props, hover, pressed, focused, focus_visible, disabled, selected, error, sizes",
            ));
        };
        let def = defs.entry(name.clone()).or_default();
        for (field, v) in fields {
            let path = format!("{group}.{name}.{field}");
            match field.as_str() {
                "extends" => match v {
                    Json::Null => def.extends = None,
                    Json::String(s) => def.extends = Some(s.clone()),
                    _ => {
                        return Err(bad_param(
                            format!("{path} must be a type name or null"),
                            "e.g. \"Button\"",
                        ));
                    }
                },
                "props" => edit_props(&path, &mut def.props, v)?,
                "hover" => edit_props(&path, &mut def.hover, v)?,
                "pressed" => edit_props(&path, &mut def.pressed, v)?,
                "focused" => edit_props(&path, &mut def.focused, v)?,
                "focus_visible" => edit_props(&path, &mut def.focus_visible, v)?,
                "disabled" => edit_props(&path, &mut def.disabled, v)?,
                "selected" => edit_props(&path, &mut def.selected, v)?,
                "error" => edit_props(&path, &mut def.error, v)?,
                "sizes" => {
                    let Some(sizes) = v.as_object() else {
                        return Err(bad_param(
                            format!("{path} must be an object keyed by size"),
                            "e.g. { \"sm\": { \"height\": 32 } }",
                        ));
                    };
                    for (size, layer) in sizes {
                        if layer.is_null() {
                            def.sizes.remove(size);
                        } else {
                            edit_props(
                                &format!("{path}.{size}"),
                                def.sizes.entry(size.clone()).or_default(),
                                layer,
                            )?;
                        }
                    }
                }
                other => {
                    return Err(bad_param(
                        format!("{group}.{name}: `{other}` is not a type field"),
                        "fields: extends, props, hover, pressed, focused, focus_visible, disabled, selected, error, sizes",
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Applies the `tokens`, `types` and `variations` edits of a call to `file`.
fn apply_edits(file: &mut ThemeFile, params: &Json) -> Result<(), UiBridgeError> {
    if let Some(tokens) = params.get("tokens").filter(|v| !v.is_null()) {
        let Some(map) = tokens.as_object() else {
            return Err(bad_param(
                "`tokens` must be an object",
                "e.g. { \"color.accent\": \"#e8793a\" }",
            ));
        };
        for (key, v) in map {
            match token_from_json(key, v)? {
                Some(value) => {
                    file.tokens.insert(key.clone(), value);
                }
                None => {
                    file.tokens.remove(key);
                }
            }
        }
    }
    if let Some(types) = params.get("types").filter(|v| !v.is_null()) {
        edit_defs("types", &mut file.types, types)?;
    }
    if let Some(variations) = params.get("variations").filter(|v| !v.is_null()) {
        edit_defs("variations", &mut file.variations, variations)?;
    }
    Ok(())
}

/// Key paths that differ between two versions of a theme file.
fn changed_keys(before: &ThemeFile, after: &ThemeFile) -> Vec<String> {
    let mut out = Vec::new();
    let token_keys: std::collections::BTreeSet<&String> =
        before.tokens.keys().chain(after.tokens.keys()).collect();
    for key in token_keys {
        if before.tokens.get(key) != after.tokens.get(key) {
            out.push(format!("tokens.{key}"));
        }
    }
    for (group, a, b) in [
        ("types", &before.types, &after.types),
        ("variations", &before.variations, &after.variations),
    ] {
        let names: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
        for name in names {
            if a.get(name) != b.get(name) {
                out.push(format!("{group}.{name}"));
            }
        }
    }
    for (field, differs) in [
        ("extends", before.extends != after.extends),
        ("scheme", before.scheme != after.scheme),
        ("density", before.density != after.density),
        ("densities", before.densities != after.densities),
        ("fonts", before.fonts != after.fonts),
    ] {
        if differs {
            out.push(field.to_owned());
        }
    }
    out
}

/// The version `reset` goes back to: the last good file read from disk, else the built-in.
fn original_of(world: &World, name: &str) -> Option<ThemeFile> {
    if let Some(file) = world
        .resource::<ThemeFiles>()
        .source_of(name)
        .and_then(|s| s.last_good.clone())
    {
        return Some(file);
    }
    BUILTIN_THEME_SOURCES
        .iter()
        .find(|(n, _)| *n == name)
        .and_then(|(n, src)| parse_theme_ron(src, n).ok())
}

/// Writes `text` next to `path` and renames it into place.
fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("theme.theme.ron");
    let tmp = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// `ui.theme_set`.
fn theme_set(world: &mut World, params: &Json) -> Result<Json, UiBridgeError> {
    let name = theme_param(world, params)?;
    let reset = bool_param(params, "reset")?.unwrap_or(false);
    let persist = bool_param(params, "persist")?.unwrap_or(false);
    let set = world.resource::<UiThemes>().set.clone();
    let Some(current) = set.get(&name).cloned() else {
        return Err(unknown_theme(&name, &set));
    };
    let mut next_file = if reset {
        original_of(world, &name).ok_or_else(|| {
            error(
                "rejected",
                "ui_theme_no_original",
                format!("theme `{name}` was not read from a file, so there is nothing to reset to"),
                "reset works for built-in themes and themes loaded from .theme.ron files",
            )
        })?
    } else {
        current.clone()
    };
    apply_edits(&mut next_file, params)?;
    if next_file.name != name {
        return Err(bad_param(
            "a theme cannot be renamed through ui.theme_set",
            "create a new .theme.ron instead",
        ));
    }
    let mut next_set = set.clone();
    next_set
        .insert(next_file.clone())
        .map_err(|e| invalid_theme(&e))?;
    let lint = lint_file(&next_file, &next_set);

    let persisted = if persist {
        let Some(path) = world
            .resource::<ThemeFiles>()
            .path_of(&name)
            .map(Path::to_path_buf)
        else {
            return Err(error(
                "rejected",
                "ui_theme_not_persistable",
                format!("theme `{name}` has no file on disk to write"),
                "only themes loaded from a .theme.ron file can be persisted; built-ins are engine assets",
            ));
        };
        let text = next_file.to_ron_string().map_err(|e| {
            error(
                "rejected",
                "ui_theme_write_failed",
                format!("cannot serialise theme `{name}`: {e}"),
                "report this as a bug",
            )
        })?;
        write_atomically(&path, &text).map_err(|e| {
            error(
                "rejected",
                "ui_theme_write_failed",
                format!(
                    "cannot write {}: {e}",
                    world.resource::<ThemeFiles>().shown(&path)
                ),
                "check the file is writable; nothing was applied",
            )
        })?;
        let mut files = world.resource_mut::<ThemeFiles>();
        if let Some(source) = files.sources.iter_mut().find(|s| s.path == path) {
            source.stamp = stamp_of(&path);
            source.last_good = Some(next_file.clone());
            source.error = None;
        }
        Some(files.shown(&path))
    } else {
        None
    };

    let changed = changed_keys(&current, &next_file);
    let now = generation(world);
    let applies = if next_set != set {
        world.resource_mut::<UiThemes>().set = next_set;
        now + 1
    } else {
        now
    };
    Ok(json!({
        "theme": name,
        "changed": changed,
        "warnings": lint.findings.iter().map(finding_json).collect::<Vec<_>>(),
        "generation": now,
        "applies_at_generation": applies,
        "persisted": persisted,
    }))
}

fn selection_json(a: &ActiveTheme) -> Json {
    json!({
        "theme": a.name,
        "density": a.density.map(Density::key),
        "high_contrast": a.high_contrast,
        "reduced_motion": a.reduced_motion,
    })
}

/// `ui.theme_select`.
fn theme_select(world: &mut World, params: &Json) -> Result<Json, UiBridgeError> {
    let Some(name) = str_param(params, "theme")? else {
        return Err(error(
            "invalid_params",
            "ui_theme_required",
            "`theme` is required",
            "ui.theme_list names every loaded theme",
        ));
    };
    {
        let set = &world.resource::<UiThemes>().set;
        if set.get(&name).is_none() {
            return Err(unknown_theme(&name, set));
        }
    }
    let previous = world.resource::<ActiveTheme>().clone();
    let mut next = previous.clone();
    next.name = name;
    if params.get("density").is_some() {
        next.density = density_param(params)?;
    }
    if let Some(hc) = bool_param(params, "high_contrast")? {
        next.high_contrast = hc;
    }
    if let Some(rm) = bool_param(params, "reduced_motion")? {
        next.reduced_motion = rm;
    }
    let now = generation(world);
    let applies = if next != previous {
        *world.resource_mut::<ActiveTheme>() = next.clone();
        now + 1
    } else {
        now
    };
    Ok(json!({
        "previous": selection_json(&previous),
        "selected": selection_json(&next),
        "generation": now,
        "applies_at_generation": applies,
    }))
}

/// `ui.theme_lint`.
fn theme_lint(world: &mut World, params: &Json) -> Result<Json, UiBridgeError> {
    let name = theme_param(world, params)?;
    let set = &world.resource::<UiThemes>().set;
    let Some(file) = set.get(&name) else {
        return Err(unknown_theme(&name, set));
    };
    Ok(lint_json(&lint_file(file, set)))
}

/// `ui.theme_reload`.
fn theme_reload(world: &mut World, params: &Json) -> Result<Json, UiBridgeError> {
    let wanted = str_param(params, "theme")?;
    let indices: Vec<usize> = {
        let files = world.resource::<ThemeFiles>();
        files
            .sources
            .iter()
            .enumerate()
            .filter(|(_, s)| wanted.is_none() || s.theme == wanted)
            .map(|(i, _)| i)
            .collect()
    };
    let mut skipped = Vec::new();
    if let Some(name) = &wanted
        && indices.is_empty()
    {
        let set = &world.resource::<UiThemes>().set;
        if set.get(name).is_none() {
            return Err(unknown_theme(name, set));
        }
        let builtin = BUILTIN_THEME_SOURCES.iter().any(|(n, _)| *n == name.as_str());
        skipped.push(json!({ "theme": name, "reason": if builtin { "built_in" } else { "no_file" } }));
    }
    let now = generation(world);
    let mut reloaded = Vec::new();
    let mut failed = Vec::new();
    let mut any_change = false;
    let outcomes = reload_in_world(world, &indices);
    let files = world.resource::<ThemeFiles>();
    for (i, outcome) in outcomes {
        let source = &files.sources[i];
        let path = files.shown(&source.path);
        match outcome {
            ReloadOutcome::Loaded { theme, changed } => {
                any_change |= changed;
                reloaded.push(json!({ "theme": theme, "path": path, "changed": changed }));
            }
            ReloadOutcome::Failed(issue) => failed.push(json!({
                "theme": source.theme,
                "path": path,
                "key": issue.key,
                "message": issue.message,
            })),
        }
    }
    Ok(json!({
        "reloaded": reloaded,
        "failed": failed,
        "skipped": skipped,
        "generation": now,
        "applies_at_generation": if any_change { now + 1 } else { now },
    }))
}

/// `wait_until` predicate `{ predicate: "ui.theme_generation", at_least: n }` (also
/// `{ op, value }` with `gte` (default), `gt`, `eq`, `lt`, `lte`).
fn theme_generation(world: &mut World, params: &Json) -> Result<Json, UiBridgeError> {
    let now = generation(world);
    let (op, want) = match (params.get("at_least"), params.get("value")) {
        (Some(v), _) => ("gte", v),
        (None, Some(v)) => (params.get("op").and_then(Json::as_str).unwrap_or("gte"), v),
        (None, None) => {
            return Err(bad_param(
                "`at_least` (or `op` and `value`) is required",
                "e.g. { predicate: \"ui.theme_generation\", at_least: 3 }",
            ));
        }
    };
    let Some(want) = want.as_u64() else {
        return Err(bad_param(
            "the generation to compare with must be a non-negative integer",
            "take it from applies_at_generation",
        ));
    };
    let holds = match op {
        "gte" => now >= want,
        "gt" => now > want,
        "eq" => now == want,
        "lt" => now < want,
        "lte" => now <= want,
        other => {
            return Err(bad_param(
                format!("`op` must be gte, gt, eq, lt or lte, not `{other}`"),
                "omit it for gte",
            ));
        }
    };
    Ok(json!({
        "holds": holds,
        "detail": format!("ui.theme_generation = {now}, {op} {want}"),
        "generation": now,
    }))
}

// ---------------------------------------------------------------------------------------------
// Preview.
// ---------------------------------------------------------------------------------------------

/// A GPU device kept for previews in apps that have none of their own.
#[derive(Resource, Clone)]
pub struct ThemePreviewDevice(pub regia_render::RenderDevice);

/// Picks the theme to show for `scheme`: `name` itself when it has that scheme, else its
/// `-dark` / `-light` sibling, else the built-in of that scheme.
fn theme_for_scheme(set: &ThemeSet, name: &str, scheme: Scheme) -> String {
    if chain_scheme(set, name) == scheme {
        return name.to_owned();
    }
    let (from, to) = match scheme {
        Scheme::Dark => ("-light", "-dark"),
        Scheme::Light => ("-dark", "-light"),
    };
    let mut candidates = Vec::new();
    if let Some(stem) = name.strip_suffix(from) {
        candidates.push(format!("{stem}{to}"));
    }
    candidates.push(format!("{name}{to}"));
    for c in candidates {
        if set.get(&c).is_some() && chain_scheme(set, &c) == scheme {
            return c;
        }
    }
    match scheme {
        Scheme::Dark => DEFAULT_THEME.to_owned(),
        Scheme::Light => "regia-light".to_owned(),
    }
}

fn size_param(params: &Json) -> Result<[u32; 2], UiBridgeError> {
    match params.get("size").filter(|v| !v.is_null()) {
        None => Ok(PREVIEW_DEFAULT_SIZE),
        Some(Json::Array(list)) if list.len() == 2 => {
            let side = |v: &Json| {
                v.as_u64()
                    .filter(|n| (1..=u64::from(PREVIEW_MAX_SIDE)).contains(n))
                    .map(|n| n as u32)
            };
            match (side(&list[0]), side(&list[1])) {
                (Some(w), Some(h)) => Ok([w, h]),
                _ => Err(bad_param(
                    format!("`size` sides must be integers from 1 to {PREVIEW_MAX_SIDE}"),
                    "e.g. [1280, 720]",
                )),
            }
        }
        Some(_) => Err(bad_param("`size` must be [width, height]", "e.g. [1280, 720]")),
    }
}

/// The safe-area profile of a preview `device`.
fn device_safe_area(device: &str, size: [u32; 2]) -> Result<regia_platform::SafeArea, UiBridgeError> {
    let fsize = [size[0] as f32, size[1] as f32];
    match device {
        "desktop" => Ok(regia_platform::SafeArea::default()),
        "tv" => Ok(crate::output::default_safe_area(
            crate::output::UiLayoutVariant::Landscape,
            fsize,
        )),
        "phone" => Ok(crate::output::default_safe_area(
            crate::output::UiLayoutVariant::for_size(fsize[0], fsize[1]),
            fsize,
        )),
        other => Err(bad_param(
            format!("`device` must be desktop, tv or phone, not `{other}`"),
            "desktop has no inset; tv is the 5 % title-safe area; phone uses the portrait or landscape render-out profile",
        )),
    }
}

/// What to preview: everything `ui.theme_preview` takes, parsed.
#[derive(Debug, Clone)]
struct PreviewRequest {
    themes: UiThemes,
    active: ActiveTheme,
    page: String,
    size: [u32; 2],
    device: String,
}

/// Builds the preview selection from the live themes plus the call's overrides; the live
/// resources are only read.
fn preview_request(world: &World, params: &Json) -> Result<PreviewRequest, UiBridgeError> {
    let mut themes = world.resource::<UiThemes>().clone();
    let mut name = theme_param(world, params)?;
    if let Some(scheme) = scheme_param(params)? {
        name = theme_for_scheme(&themes.set, &name, scheme);
    }
    if params.get("tokens").is_some_and(|v| !v.is_null())
        || params.get("types").is_some_and(|v| !v.is_null())
        || params.get("variations").is_some_and(|v| !v.is_null())
    {
        let Some(mut file) = themes.set.get(&name).cloned() else {
            return Err(unknown_theme(&name, &themes.set));
        };
        apply_edits(&mut file, params)?;
        themes.set.insert(file).map_err(|e| invalid_theme(&e))?;
    }
    let mut active = world.get_resource::<ActiveTheme>().cloned().unwrap_or_default();
    active.name = name;
    active.high_contrast = false;
    active.reduced_motion = true;
    if params.get("density").is_some() {
        active.density = density_param(params)?;
    }
    let size = size_param(params)?;
    let device = str_param(params, "device")?.unwrap_or_else(|| "desktop".to_owned());
    device_safe_area(&device, size)?;
    Ok(PreviewRequest {
        themes,
        active,
        page: str_param(params, "page")?.unwrap_or_else(|| THEME_SHEET_PAGE.to_owned()),
        size,
        device,
    })
}

/// Reads a `.ui.ron` page: an `assets://` path through the app's VFS, else a file path.
fn read_page(world: &World, page: &str) -> Result<String, UiBridgeError> {
    let not_found = |why: String| {
        error(
            "not_found",
            "ui_theme_page_not_found",
            format!("cannot read page `{page}`: {why}"),
            "page is \"theme-sheet\", an assets:// path or a file path to a .ui.ron",
        )
    };
    if page.contains("://") {
        let vfs = world
            .get_resource::<regia_platform::Vfs>()
            .ok_or_else(|| not_found("the app has no VFS".to_owned()))?;
        let path = regia_platform::VfsPath::parse(page).map_err(|e| not_found(e.to_string()))?;
        let bytes = vfs.read_sync(&path).map_err(|e| not_found(e.to_string()))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        std::fs::read_to_string(page).map_err(|e| not_found(e.to_string()))
    }
}

/// A headless UI app for one preview: single-threaded (it runs inside the caller's frame) and on a
/// fixed step.
fn preview_app(request: &PreviewRequest) -> App {
    let mut app = App::new();
    app.add_plugins(crate::UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    {
        let world = app.world_mut();
        {
            let mut schedules = world.resource_mut::<bevy_ecs::schedule::Schedules>();
            for (_, schedule) in schedules.iter_mut() {
                schedule.set_executor(bevy_ecs::schedule::SingleThreadedExecutor::new());
            }
        }
        world.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(16),
        ));
        world.insert_resource(request.themes.clone());
        world.insert_resource(request.active.clone());
        world.insert_resource(UiScales {
            window_size: [request.size[0] as f32, request.size[1] as f32],
            scale_factor: 1.0,
        });
        if let Some(mut files) = world.get_resource_mut::<ThemeFiles>() {
            files.watch = ThemeWatch::Off;
        }
    }
    app
}

/// Renders a preview and returns `(width, height, rgba)`: tightly packed sRGB RGBA8.
fn render_preview(world: &mut World, request: &PreviewRequest) -> Result<(u32, u32, Vec<u8>), UiBridgeError> {
    let device = match world.get_resource::<regia_render::RenderDevice>() {
        Some(device) => device.clone(),
        None => match world.get_resource::<ThemePreviewDevice>() {
            Some(cached) => cached.0.clone(),
            None => {
                let device = regia_render::RenderDevice::headless().map_err(|e| {
                    error(
                        "unsupported",
                        "ui_theme_preview_no_gpu",
                        format!("no GPU for the offscreen preview: {e}"),
                        "install lavapipe (Vulkan software) or a GPU driver",
                    )
                })?;
                world.insert_resource(ThemePreviewDevice(device.clone()));
                device
            }
        },
    };
    let page_text = if request.page == THEME_SHEET_PAGE {
        None
    } else {
        Some(read_page(world, &request.page)?)
    };
    let migrators = world
        .get_resource::<crate::UiMigrators>()
        .cloned()
        .unwrap_or_default();

    let mut app = preview_app(request);
    {
        let world = app.world_mut();
        match &page_text {
            None => {
                spawn_theme_sheet(world);
            }
            Some(text) => {
                let file = crate::parse_ui_ron(text, &request.page, &migrators).map_err(|e| {
                    error(
                        "invalid_params",
                        "ui_theme_page_invalid",
                        e.to_string(),
                        "the page must be a .ui.ron layout that loads with the game",
                    )
                })?;
                const NAME: &str = "__theme_preview";
                world
                    .resource_mut::<crate::UiLibrary>()
                    .templates
                    .insert(NAME.to_owned(), file);
                crate::authoring::spawn_ui(world, NAME).map_err(|e| {
                    error(
                        "invalid_params",
                        "ui_theme_page_invalid",
                        e.to_string(),
                        "check the page's root",
                    )
                })?;
            }
        }
    }
    for _ in 0..PREVIEW_FRAMES {
        app.update();
    }
    let [width, height] = request.size;
    let world = app.world_mut();
    let safe = device_safe_area(&request.device, request.size)?;
    world.resource_mut::<crate::output::UiOutput>().safe_area = Some(safe);
    let spec = regia_render::RenderOutSpec::new(width, height, regia_render::HudLayout::Auto);
    crate::output::begin_render_out(world, &spec);
    let backdrop = world
        .resource::<ThemeResolver>()
        .resolved_theme()
        .and_then(|t| t.color("color.bg"))
        .unwrap_or(UiColor::BLACK);
    let shared = world.resource::<SharedUiRenderer>().clone();
    let target = OffscreenTarget::new(&device, width, height);
    target.clear(&device, backdrop.0.map(f64::from));
    target.draw_ui(&device, &mut shared.lock());
    let rgba = target.read(&device).map_err(|e| {
        error(
            "rejected",
            "ui_theme_preview_failed",
            format!("offscreen readback failed: {e}"),
            "retry; report it if it repeats",
        )
    })?;
    Ok((width, height, rgba))
}

/// Renders the theme preview a `ui.theme_preview` call describes, without touching the live UI.
/// Returns `(width, height, rgba)` (tightly packed sRGB RGBA8). The agent crate turns the pixels
/// into a PNG file and bytes.
pub fn preview_pixels(world: &mut World, params: &Json) -> Result<(u32, u32, Vec<u8>), UiBridgeError> {
    world.init_resource::<ThemeFiles>();
    let request = preview_request(world, params)?;
    render_preview(world, &request)
}

/// `ui.theme_preview`: the host-level answer carries the raw pixels as `rgba_base64`; the agent
/// crate replaces them with `png_base64` and a file `path`.
fn theme_preview(world: &mut World, params: &Json) -> Result<Json, UiBridgeError> {
    let _span = tracing::info_span!("ui_theme_preview").entered();
    let request = preview_request(world, params)?;
    let (width, height, rgba) = render_preview(world, &request)?;
    Ok(json!({
        "width": width,
        "height": height,
        "theme": request.active.name,
        "page": request.page,
        "scheme": scheme_name(chain_scheme(&request.themes.set, &request.active.name)),
        "density": request.active.density.unwrap_or_else(|| chain_density(&request.themes.set, &request.active.name)).key(),
        "device": request.device,
        "generation": generation(world),
        "rgba_base64": base64(&rgba),
    }))
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard padded base64 (RFC 4648 § 4).
fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(BASE64[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The theme sheet.
// ---------------------------------------------------------------------------------------------

/// Button rows of the sheet: classes per row.
const SHEET_BUTTONS: [&[&str]; 4] = [
    &["button"],
    &["button", "primary"],
    &["button", "danger"],
    &["button", "ghost"],
];

/// Widget states shown per button row.
const SHEET_STATES: [&str; 6] = ["normal", "hovered", "pressed", "focused", "selected", "disabled"];

fn sheet_style(direction: FlexDirection, gap: f32, padding: f32) -> NodeStyle {
    NodeStyle::new(Style {
        direction,
        padding: Edges::px(padding),
        gap: [Dimension::Points(gap), Dimension::Points(gap)],
        align_items: Align::Center,
        flex_wrap: true,
        ..Default::default()
    })
}

fn sheet_child(world: &mut World, parent: Entity, bundle: impl Bundle) -> Entity {
    let child = world.spawn(bundle).id();
    world.entity_mut(parent).add_child(child);
    child
}

fn sheet_label(world: &mut World, parent: Entity, text: &str) -> Entity {
    sheet_child(world, parent, (UiNode, Text::new(text)))
}

fn mark_state(world: &mut World, entity: Entity, state: &str) {
    let mut e = world.entity_mut(entity);
    match state {
        "hovered" => {
            e.insert(Hovered);
        }
        "pressed" => {
            e.insert((Hovered, Pressed));
        }
        "focused" => {
            e.insert(Focused);
        }
        "selected" => {
            e.insert(Selected);
        }
        "disabled" => {
            e.insert(Disabled);
        }
        _ => {}
    }
}

/// Spawns the built-in theme sheet (`page: "theme-sheet"`) as a new root: every button variation
/// in every state, toggles, a slider, a progress bar, list rows and text on a surface. State
/// markers are set directly (the nodes are not interactable), so the sheet shows hover, pressed
/// and focus without a pointer. Returns the root.
pub fn spawn_theme_sheet(world: &mut World) -> Entity {
    let root = world
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                padding: Edges::px(24.0),
                gap: [Dimension::Points(16.0), Dimension::Points(16.0)],
                ..Default::default()
            }),
            SafeAreaPadding,
            NodeName(THEME_SHEET_PAGE.to_owned()),
        ))
        .id();

    let buttons = sheet_child(
        world,
        root,
        (
            UiNode,
            sheet_style(FlexDirection::Column, 8.0, 16.0),
            Classes(vec!["surface".into()]),
        ),
    );
    for classes in SHEET_BUTTONS {
        let row = sheet_child(
            world,
            buttons,
            (UiNode, sheet_style(FlexDirection::Row, 8.0, 0.0)),
        );
        for state in SHEET_STATES {
            let button = sheet_child(
                world,
                row,
                (
                    UiNode,
                    NodeStyle::new(Style {
                        size: [Dimension::Points(120.0), Dimension::Points(44.0)],
                        justify_content: JustifyContent::Center,
                        align_items: Align::Center,
                        ..Default::default()
                    }),
                    Button,
                    Classes(classes.iter().map(|c| (*c).to_owned()).collect()),
                    Themed::default(),
                    WidgetSize::Md,
                ),
            );
            mark_state(world, button, state);
            sheet_label(world, button, state);
        }
    }

    let controls = sheet_child(
        world,
        root,
        (
            UiNode,
            sheet_style(FlexDirection::Row, 16.0, 16.0),
            Classes(vec!["surface".into()]),
        ),
    );
    for checked in [false, true] {
        sheet_child(
            world,
            controls,
            (
                UiNode,
                NodeStyle::new(Style {
                    size: [Dimension::Points(64.0), Dimension::Points(32.0)],
                    ..Default::default()
                }),
                Toggle { checked },
                Classes(vec!["toggle".into()]),
                Themed::default(),
            ),
        );
    }
    sheet_child(
        world,
        controls,
        (
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(240.0), Dimension::Points(28.0)],
                ..Default::default()
            }),
            Slider {
                value: 0.6,
                ..Default::default()
            },
            Classes(vec!["slider-track".into()]),
            Themed::default(),
        ),
    );
    sheet_child(
        world,
        controls,
        (
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(240.0), Dimension::Points(16.0)],
                ..Default::default()
            }),
            ProgressBar { value: 0.35 },
            Classes(vec!["surface".into()]),
            Themed::default(),
        ),
    );

    let lists = sheet_child(
        world,
        root,
        (
            UiNode,
            sheet_style(FlexDirection::Column, 4.0, 16.0),
            Classes(vec!["surface".into()]),
        ),
    );
    for state in ["normal", "hovered", "selected"] {
        let row = sheet_child(
            world,
            lists,
            (
                UiNode,
                NodeStyle::new(Style {
                    size: [Dimension::Points(480.0), Dimension::Points(44.0)],
                    padding: Edges::px(12.0),
                    align_items: Align::Center,
                    ..Default::default()
                }),
                Classes(vec!["list-item".into()]),
                Themed::default(),
            ),
        );
        mark_state(world, row, state);
        sheet_label(world, row, &format!("List row ({state})"));
    }
    sheet_label(world, lists, "Body text on a surface");
    root
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(crate::UiPlugin::code_only());
        app.add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::WindowScaleFactorChanged>()
            .add_message::<regia_platform::CursorMoved>()
            .add_message::<regia_platform::MouseButtonInput>()
            .add_message::<regia_platform::TouchInput>()
            .add_message::<regia_platform::MouseWheel>()
            .add_message::<regia_platform::KeyboardInput>()
            .add_message::<regia_platform::ImeEvent>();
        app.update();
        app
    }

    #[test]
    fn globs_match_like_shell_patterns() {
        assert!(glob_match("color.*", "color.accent"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("color.accent-?over", "color.accent-hover"));
        assert!(glob_match("*.md", "radius.md"));
        assert!(!glob_match("color.*", "radius.md"));
        assert!(!glob_match("color.accent", "color.accent-hover"));
    }

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn get_shows_raw_and_resolved_values_side_by_side() {
        let mut app = app();
        let out = call(
            app.world_mut(),
            "ui.theme_get",
            &json!({ "keys": "color.accent*" }),
        )
        .expect("get");
        assert_eq!(out["name"], DEFAULT_THEME);
        assert_eq!(out["tokens"]["color.accent"]["raw"], "#3d9df0");
        assert_eq!(out["tokens"]["color.accent"]["value"], "#3d9df0");
        assert_eq!(
            out["tokens"]["color.accent-hover"]["raw"],
            "lighten(@color.accent, 0.06)"
        );
        assert_eq!(out["tokens"]["color.accent-hover"]["value"], "#5ab0ff");
        assert!(out["tokens"].get("radius.md").is_none(), "the glob filters keys");
        let light = call(
            app.world_mut(),
            "ui.theme_get",
            &json!({ "theme": "regia-light", "keys": ["color.text"], "resolved": false }),
        )
        .expect("get");
        assert_eq!(light["tokens"]["color.text"]["from"], "regia-light");
        assert!(light["tokens"]["color.text"].get("value").is_none());
    }

    /// ADR-9340: `ui.theme_get` reports the caption `subtitle` style the caption rows draw with
    /// and, for a type, the layout box the theme moves (`layout`; a `WidgetSize` token height
    /// resolves in `height` but does not move layout).
    #[test]
    fn get_reports_the_caption_style_and_the_layout_fields() {
        let mut app = app();
        let out = call(app.world_mut(), "ui.theme_get", &json!({ "keys": [] })).expect("get");
        let caption = &out["caption"];
        assert_eq!(caption["text"], "#ffffff", "{caption}");
        assert_eq!(caption["plate"], "#000000");
        assert_eq!(caption["outline"], "#000000");
        assert_eq!(caption["outline_width"], json!(2.0));
        assert_eq!(caption["size"], json!(1.0));
        assert!(caption["font"].is_null(), "the bundled theme keeps the UI font");
        assert!(caption["plate_opacity"].is_null());
        let button = call(
            app.world_mut(),
            "ui.theme_get",
            &json!({ "type": "Button", "keys": [], "size": "md" }),
        )
        .expect("get");
        let props = &button["resolved"]["props"];
        assert_eq!(props["height"], json!(44.0), "the md control height token");
        assert_eq!(
            props["layout"],
            json!({ "height": null, "padding_x": null, "gap": null }),
            "no theme property moves the button's layout"
        );
    }

    #[test]
    fn get_resolves_a_type_and_state_through_the_resolver() {
        let mut app = app();
        let out = call(
            app.world_mut(),
            "ui.theme_get",
            &json!({ "type": "Button.primary", "keys": [] , "state": "hovered" }),
        )
        .expect("get");
        assert_eq!(out["resolved"]["chain"], json!(["Button", "Button.primary"]));
        assert_eq!(out["resolved"]["props"]["background"], "#5ab0ff");
        assert_eq!(out["resolved"]["props"]["text_color"], "#0b1220");
        let missing =
            call(app.world_mut(), "ui.theme_get", &json!({ "type": "Nope" })).expect_err("unknown type");
        assert_eq!(missing.reason, "ui_theme_type_unknown");
    }

    /// `.theme.ron` v2 (ADR-8105 (e)): `ui.theme_get` lists the chain's style rules by selector
    /// text with their specificity, and `ui.theme_set` edits a type's `focus_visible` layer.
    #[test]
    fn get_lists_style_rules_and_set_edits_the_focus_visible_layer() {
        let mut app = app();
        app.world_mut()
            .resource_mut::<UiThemes>()
            .set
            .load_ron(
                r#"(format_version: 2, name: "rules", extends: Some("regia-dark"), rules: [
                    (inside: Some("Toolbar"), widget: Some("Button"), props: { "radius": 2 }),
                    (classes: ["hot"], state: Some(hover), props: { "radius": 4 }),
                ])"#,
                "rules.theme.ron",
            )
            .expect("theme");
        let out = call(
            app.world_mut(),
            "ui.theme_get",
            &json!({ "theme": "rules", "keys": [] }),
        )
        .expect("get");
        assert_eq!(out["format_version"], 2);
        assert_eq!(out["rules"][0]["selector"], "Toolbar Button");
        assert_eq!(out["rules"][0]["specificity"], json!([0, 2]));
        assert_eq!(out["rules"][1]["selector"], "[hot] :hover");
        assert_eq!(out["rules"][1]["state"], "hover");
        assert_eq!(out["rules"][1]["props"]["radius"], json!(4.0));
        call(
            app.world_mut(),
            "ui.theme_set",
            &json!({ "theme": "rules", "types": { "Button": { "focus_visible": { "border_width": 5 } } } }),
        )
        .expect("set");
        let file = app
            .world()
            .resource::<UiThemes>()
            .set
            .get("rules")
            .cloned()
            .expect("file");
        assert!(file.types["Button"].focus_visible.contains_key("border_width"));
    }

    #[test]
    fn theme_get_touch_state_skips_hover() {
        let mut app = app();
        let mut look = |state: Json| {
            call(
                app.world_mut(),
                "ui.theme_get",
                &json!({ "type": "Button", "keys": [], "state": state }),
            )
            .expect("theme_get")
        };
        let normal = look(json!("normal"));
        let hovered = look(json!("hovered"));
        let touch = look(json!(["hovered", "touch"]));
        assert_eq!(touch["resolved"]["state"], json!(["hovered", "touch"]));
        assert_eq!(
            touch["resolved"]["props"]["background"], normal["resolved"]["props"]["background"],
            "touch suppresses the hover background"
        );
        assert_ne!(
            touch["resolved"]["props"]["background"],
            hovered["resolved"]["props"]["background"]
        );
        assert_eq!(
            touch["resolved"]["props"]["border_color"],
            normal["resolved"]["props"]["border_color"]
        );
        assert_ne!(
            hovered["resolved"]["props"]["border_color"],
            normal["resolved"]["props"]["border_color"]
        );
    }

    #[test]
    fn theme_get_omitted_density_uses_active_density() {
        let mut app = app();
        let size = |world: &mut World, theme: &str| {
            call(
                world,
                "ui.theme_get",
                &json!({ "theme": theme, "keys": "size.control.md" }),
            )
            .expect("theme_get")
        };
        let comfortable = size(app.world_mut(), "regia-light");
        assert_eq!(comfortable["tokens"]["size.control.md"]["value"], json!(44.0));

        app.world_mut().resource_mut::<ActiveTheme>().density = Some(Density::Compact);
        app.update();
        let compact = size(app.world_mut(), "regia-light");
        assert_eq!(compact["density"], "compact");
        assert_eq!(compact["tokens"]["size.control.md"]["value"], json!(28.0));

        // High contrast resolves a different theme name. The omitted density still follows
        // the active density, including when it comes from `UiDensity` alone.
        {
            let world = app.world_mut();
            let mut active = world.resource_mut::<ActiveTheme>();
            active.high_contrast = true;
            active.density = None;
            world.insert_resource(crate::widget_style::UiDensity::Compact);
        }
        app.update();
        let contrasted = size(app.world_mut(), "regia-dark");
        assert_eq!(contrasted["tokens"]["size.control.md"]["value"], json!(28.0));
        assert_eq!(contrasted["density"], "compact");
    }

    #[test]
    fn set_applies_next_frame_and_returns_changes_and_lint() {
        let mut app = app();
        let before = generation(app.world());
        let out = call(
            app.world_mut(),
            "ui.theme_set",
            &json!({ "tokens": { "color.text": "#4d4d4d" } }),
        )
        .expect("set");
        assert_eq!(out["changed"], json!(["tokens.color.text"]));
        assert_eq!(out["applies_at_generation"], before + 1);
        assert!(
            out["warnings"]
                .as_array()
                .expect("list")
                .iter()
                .any(|w| w["kind"] == "contrast" && w["pair"] == json!(["color.text", "color.surface"]))
        );
        assert_eq!(
            generation(app.world()),
            before,
            "nothing changes before the frame"
        );
        app.update();
        assert_eq!(generation(app.world()), before + 1);
        let holds = call(
            app.world_mut(),
            "ui.theme_generation",
            &json!({ "at_least": before + 1 }),
        )
        .expect("predicate");
        assert_eq!(holds["holds"], true);
        // The same edit again changes nothing and costs no generation.
        let again = call(
            app.world_mut(),
            "ui.theme_set",
            &json!({ "tokens": { "color.text": "#4d4d4d" } }),
        )
        .expect("set");
        assert_eq!(again["changed"], json!([]));
        assert_eq!(again["applies_at_generation"], before + 1);
    }

    #[test]
    fn set_rejects_a_broken_value_with_its_key_path_and_applies_nothing() {
        let mut app = app();
        let set_before = app.world().resource::<UiThemes>().set.clone();
        let err = call(
            app.world_mut(),
            "ui.theme_set",
            &json!({ "tokens": { "color.accent-hover": "lighten(@color.nope, 0.1)" } }),
        )
        .expect_err("unknown reference");
        assert_eq!(err.reason, "ui_theme_invalid");
        assert!(
            err.message.starts_with("tokens.color.accent-hover"),
            "{}",
            err.message
        );
        assert_eq!(app.world().resource::<UiThemes>().set, set_before);
        let built_in = call(
            app.world_mut(),
            "ui.theme_set",
            &json!({ "tokens": { "radius.md": 2 }, "persist": true }),
        )
        .expect_err("no file");
        assert_eq!(built_in.reason, "ui_theme_not_persistable");
    }

    #[test]
    fn reset_returns_a_built_in_to_its_shipped_values() {
        let mut app = app();
        call(
            app.world_mut(),
            "ui.theme_set",
            &json!({ "tokens": { "radius.md": 2 } }),
        )
        .expect("set");
        let out = call(app.world_mut(), "ui.theme_set", &json!({ "reset": true })).expect("reset");
        assert_eq!(out["changed"], json!(["tokens.radius.md"]));
        let shipped = crate::theme_resolve::builtin_theme_set();
        assert_eq!(
            app.world().resource::<UiThemes>().set.get(DEFAULT_THEME),
            shipped.get(DEFAULT_THEME)
        );
    }

    #[test]
    fn select_returns_the_previous_selection_and_rejects_unknown_names() {
        let mut app = app();
        let out = call(
            app.world_mut(),
            "ui.theme_select",
            &json!({ "theme": "regia-light", "density": "compact" }),
        )
        .expect("select");
        assert_eq!(out["previous"]["theme"], DEFAULT_THEME);
        assert_eq!(out["selected"]["density"], "compact");
        assert_eq!(app.world().resource::<ActiveTheme>().name, "regia-light");
        let err = call(app.world_mut(), "ui.theme_select", &json!({ "theme": "nope" })).expect_err("unknown");
        assert_eq!(err.reason, "ui_theme_unknown");
        let list = call(app.world_mut(), "ui.theme_list", &json!({})).expect("list");
        assert_eq!(list["active"], "regia-light");
        // Every shipped theme is listed (four since the light high-contrast built-in).
        let shipped = crate::theme_resolve::builtin_theme_set().names().count();
        assert_eq!(list["themes"].as_array().map(Vec::len), Some(shipped));
        assert!(shipped >= 3);
    }

    #[test]
    fn lint_reports_clean_builtins() {
        let mut app = app();
        for theme in ["regia-dark", "regia-light", "regia-high-contrast"] {
            let out = call(app.world_mut(), "ui.theme_lint", &json!({ "theme": theme })).expect("lint");
            assert_eq!(out["clean"], true, "{theme}: {out}");
        }
    }

    /// Roadmap lane 7 acceptance (hot reload through the watch path): a `.theme.ron` loaded from disk
    /// is re-read when a `SourceAssetChanged` names it. A good edit swaps in on the next frame; a
    /// broken edit keeps the last good theme and is reported on the file's source entry.
    #[test]
    fn a_changed_theme_file_hot_reloads_and_a_broken_edit_keeps_the_last_good() {
        let mut app = app();
        let dir = regia_test_utils::unique_temp_dir("theme-hot-reload");
        let path = dir.join("mine.theme.ron");
        let body = |accent: &str| {
            format!(
                r##"(format_version: 1, name: "mine", extends: "regia-dark", tokens: {{"color.accent": "{accent}"}})"##
            )
        };
        let accent = |app: &mut App| {
            let out = call(
                app.world_mut(),
                "ui.theme_get",
                &json!({ "theme": "mine", "keys": ["color.accent"] }),
            )
            .expect("get");
            out["tokens"]["color.accent"]["value"].clone()
        };
        let source = |app: &mut App| {
            let list = call(app.world_mut(), "ui.theme_list", &json!({})).expect("list");
            list["files"]
                .as_array()
                .expect("files")
                .iter()
                .find(|s| s["theme"] == "mine")
                .cloned()
                .expect("the file is a source")
        };
        regia_test_utils::write_atomic(&path, body("#112233").as_bytes()).expect("write");
        assert_eq!(load_theme_file(app.world_mut(), &path).expect("load"), "mine");
        app.update();
        assert_eq!(accent(&mut app), "#112233");
        assert_eq!(source(&mut app)["loads"], 1);

        // A good edit, announced the way the asset watcher announces it.
        regia_test_utils::write_atomic(&path, body("#445566").as_bytes()).expect("rewrite");
        app.world_mut()
            .write_message(regia_asset::SourceAssetChanged::new(path.clone()));
        app.update();
        assert_eq!(accent(&mut app), "#445566", "the edit swapped in");
        let entry = source(&mut app);
        assert_eq!(entry["loads"], 2);
        assert_eq!(entry["error"], Json::Null);

        // A broken edit: the last good theme stays and the source reports the error.
        regia_test_utils::write_atomic(&path, b"(format_version: 1, name: ").expect("break");
        app.world_mut()
            .write_message(regia_asset::SourceAssetChanged::new(path.clone()));
        app.update();
        assert_eq!(accent(&mut app), "#445566", "the last good theme is kept");
        let entry = source(&mut app);
        assert_eq!(entry["loads"], 2);
        assert_ne!(entry["error"], Json::Null, "{entry}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_of_a_built_in_is_skipped_not_an_error() {
        let mut app = app();
        let out = call(
            app.world_mut(),
            "ui.theme_reload",
            &json!({ "theme": "regia-dark" }),
        )
        .expect("reload");
        assert_eq!(out["skipped"][0]["reason"], "built_in");
        let err = call(app.world_mut(), "ui.theme_reload", &json!({ "theme": "nope" })).expect_err("unknown");
        assert_eq!(err.reason, "ui_theme_unknown");
    }

    #[test]
    fn issues_carry_the_key_path_of_every_error_kind() {
        let e = ThemeError::UnknownReference {
            theme: "t".into(),
            key: "tokens.color.a".into(),
            reference: "color.b".into(),
        };
        assert_eq!(ThemeLoadIssue::of(&e).key, "tokens.color.a");
        let e = ThemeError::UnknownParent {
            theme: "t".into(),
            parent: "p".into(),
        };
        assert_eq!(ThemeLoadIssue::of(&e).key, "extends");
        let e = ThemeError::Cycle {
            theme: "t".into(),
            chain: "tokens.a -> tokens.b -> tokens.a".into(),
        };
        assert_eq!(ThemeLoadIssue::of(&e).key, "tokens.a");
        let e = ThemeError::Cycle {
            theme: "t".into(),
            chain: "types.Button.props.radius: tokens.a -> tokens.b -> tokens.a".into(),
        };
        assert_eq!(ThemeLoadIssue::of(&e).key, "types.Button.props.radius");
    }
}
