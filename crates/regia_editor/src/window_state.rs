// SPDX-License-Identifier: MPL-2.0
//! The main editor window's state per project (`docs/specs/19-editor.md` § 2, the v0.6 parity
//! addendum § 1.4, ADR-7753): the monitor, size, position, full-screen and maximized state, the
//! open level and the open asset-editor documents, restored on the next start of the same
//! project.
//!
//! One user file, `<user config>/editor_window_state.ron` (version 1), keyed by project path:
//!
//! ```ron
//! (
//!     format_version: 1,
//!     projects: {
//!         "/home/me/games/forest": (
//!             monitor: Some("DP-1"),
//!             position: Some((80, 40)),
//!             size: (1920, 1080),
//!             fullscreen: false,
//!             maximized: true,
//!             level: Some("assets/levels/main.level.ron"),
//!             asset_tabs: ["assets/materials/bark.material.ron"],
//!         ),
//!     },
//! )
//! ```
//!
//! The window host keeps [`ObservedWindow`] current (size, position, monitor, full screen); the
//! shell writes the record when the editor exits ([`save_for_project`]) and, when the project opens
//! again, the host places the window with [`place`] (a monitor that is gone falls back to the
//! primary display) and the shell reopens the level and queues the asset documents
//! ([`ReopenQueue`]) through the same route as a double-click in the Content Browser.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

/// The window state file, in the user config directory.
pub const WINDOW_STATE_FILE: &str = "editor_window_state.ron";
/// Current version of `editor_window_state.ron`.
pub const WINDOW_STATE_FORMAT_VERSION: u32 = 1;
/// The most projects the file remembers (the oldest entries go first).
pub const MAX_PROJECTS: usize = 64;
/// The most asset documents one record reopens.
pub const MAX_ASSET_TABS: usize = 32;
/// The smallest window size restored, physical pixels.
pub const MIN_SIZE: (u32, u32) = (320, 240);
/// How much of a window must stay on its monitor to keep its position, physical pixels.
pub const MIN_VISIBLE: i32 = 64;

/// One project's window state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowRecord {
    /// The monitor's name, when the window system names it.
    #[serde(default)]
    pub monitor: Option<String>,
    /// The outer top-left corner on the desktop, physical pixels (`None` where the window system
    /// does not tell, as on Wayland).
    #[serde(default)]
    pub position: Option<(i32, i32)>,
    /// The inner size, physical pixels.
    pub size: (u32, u32),
    /// Full screen on its monitor.
    #[serde(default)]
    pub fullscreen: bool,
    /// Maximized.
    #[serde(default)]
    pub maximized: bool,
    /// The open level, relative to the project.
    #[serde(default)]
    pub level: Option<String>,
    /// The documents open in asset editors, relative to the project.
    #[serde(default)]
    pub asset_tabs: Vec<String>,
}

/// The whole file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowStateFile {
    /// [`WINDOW_STATE_FORMAT_VERSION`] (a missing field reads as the current version).
    #[serde(default = "crate::editor_prefs::version_missing")]
    pub format_version: u32,
    /// Records by project path. The order of `order` decides which go when the file is full.
    #[serde(default)]
    pub projects: BTreeMap<String, WindowRecord>,
    /// Project paths, most recently saved last.
    #[serde(default)]
    pub order: Vec<String>,
}

impl Default for WindowStateFile {
    fn default() -> Self {
        Self {
            format_version: WINDOW_STATE_FORMAT_VERSION,
            projects: BTreeMap::new(),
            order: Vec::new(),
        }
    }
}

impl WindowStateFile {
    /// Parses the file text.
    ///
    /// # Errors
    ///
    /// Not the shape, or written by a newer editor.
    pub fn from_text(text: &str) -> Result<Self, String> {
        let mut file = crate::editor_prefs::read_versioned(text, WINDOW_STATE_FORMAT_VERSION, |f: &Self| {
            f.format_version
        })
        .map_err(|e| match e {
            crate::editor_prefs::VersionedError::Parse(m) => m,
            crate::editor_prefs::VersionedError::Newer(found) => {
                format!("written by a newer editor (format_version {found}); left untouched")
            }
        })?;
        file.format_version = WINDOW_STATE_FORMAT_VERSION;
        Ok(file)
    }

    /// The file text.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut file = self.clone();
        file.format_version = WINDOW_STATE_FORMAT_VERSION;
        ron::ser::to_string_pretty(&file, regia_core::ron_pretty().struct_names(false)).unwrap_or_default()
    }

    /// Stores `record` for `project`, dropping the oldest projects past [`MAX_PROJECTS`].
    pub fn put(&mut self, project: &str, mut record: WindowRecord) {
        record.asset_tabs.truncate(MAX_ASSET_TABS);
        self.order.retain(|p| p != project);
        self.order.push(project.to_owned());
        self.projects.insert(project.to_owned(), record);
        while self.order.len() > MAX_PROJECTS {
            let gone = self.order.remove(0);
            self.projects.remove(&gone);
        }
        // Records the order does not name (a hand-edited file) go too.
        let order = &self.order;
        self.projects.retain(|p, _| order.contains(p));
    }
}

/// The `from` versions of the registered `editor_window_state.ron` migration steps (none:
/// version 1 is the first).
#[must_use]
pub fn window_state_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape of `editor_window_state.ron` (ADR-7902: the `editor-window-state` format).
#[must_use]
pub fn window_state_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<WindowStateFile>(),
        regia_core::schema::serde_field_names::<WindowRecord>(),
    ])
}

/// The key of a project: its path as given (the shell's project root).
#[must_use]
pub fn project_key(root: &Path) -> String {
    root.display().to_string()
}

/// The file in user config directory `config_dir`.
#[must_use]
pub fn window_state_path(config_dir: &Path) -> PathBuf {
    config_dir.join(WINDOW_STATE_FILE)
}

/// Reads the file; an absent file is empty.
///
/// # Errors
///
/// The file cannot be read or does not parse.
pub fn read_file(path: &Path) -> Result<WindowStateFile, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => WindowStateFile::from_text(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(WindowStateFile::default()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// The record of project `root`, if saved (an unreadable file reads as none, with a warning).
#[must_use]
pub fn read_record(config_dir: &Path, root: &Path) -> Option<WindowRecord> {
    match read_file(&window_state_path(config_dir)) {
        Ok(file) => file.projects.get(&project_key(root)).cloned(),
        Err(err) => {
            tracing::warn!(target: "regia_editor", "window state not read: {err}");
            None
        }
    }
}

/// Stores `record` for project `root` in the file (read, update, write atomically). A newer
/// editor's file is left untouched.
///
/// # Errors
///
/// Reading or writing failed.
pub fn save_for_project(config_dir: &Path, root: &Path, record: WindowRecord) -> Result<PathBuf, String> {
    let path = window_state_path(config_dir);
    let mut file = read_file(&path)?;
    file.put(&project_key(root), record);
    std::fs::create_dir_all(config_dir)
        .map_err(|e| format!("cannot create {}: {e}", config_dir.display()))?;
    crate::level_io::write_atomic(&path, file.to_text().as_bytes())?;
    Ok(path)
}

/// The main window as the host last saw it (a resource the window host keeps current).
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct ObservedWindow {
    /// The monitor's name.
    pub monitor: Option<String>,
    /// The outer top-left corner, physical pixels.
    pub position: Option<(i32, i32)>,
    /// The inner size, physical pixels.
    pub size: (u32, u32),
    /// Full screen.
    pub fullscreen: bool,
    /// Maximized.
    pub maximized: bool,
}

/// One monitor the window system offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    /// Its name.
    pub name: Option<String>,
    /// Top-left corner on the desktop, physical pixels.
    pub position: (i32, i32),
    /// Size, physical pixels.
    pub size: (u32, u32),
    /// Whether it is the primary display.
    pub primary: bool,
}

/// Where and how to open the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// Index into the monitors given to [`place`] (`None` when there were none).
    pub monitor: Option<usize>,
    /// The outer top-left corner (`None`: let the window system place it).
    pub position: Option<(i32, i32)>,
    /// The inner size.
    pub size: (u32, u32),
    /// Full screen on `monitor`.
    pub fullscreen: bool,
    /// Maximized.
    pub maximized: bool,
}

/// Places a saved window on the monitors there are now: the record's monitor by name, else the
/// primary display (else the first). The size is kept within [`MIN_SIZE`] and the monitor; a
/// position that would leave less than [`MIN_VISIBLE`] pixels on the monitor (or a window moved to
/// another monitor) is centered on it.
#[must_use]
pub fn place(record: &WindowRecord, monitors: &[MonitorInfo]) -> Placement {
    let named = record
        .monitor
        .as_ref()
        .and_then(|name| monitors.iter().position(|m| m.name.as_ref() == Some(name)));
    let fallback = monitors
        .iter()
        .position(|m| m.primary)
        .or((!monitors.is_empty()).then_some(0));
    let index = named.or(fallback);
    let mut size = (record.size.0.max(MIN_SIZE.0), record.size.1.max(MIN_SIZE.1));
    let Some(monitor) = index.and_then(|i| monitors.get(i)) else {
        return Placement {
            monitor: None,
            position: record.position,
            size,
            fullscreen: record.fullscreen,
            maximized: record.maximized,
        };
    };
    size = (
        size.0.min(monitor.size.0.max(MIN_SIZE.0)),
        size.1.min(monitor.size.1.max(MIN_SIZE.1)),
    );
    let centered = (
        monitor.position.0 + i32::try_from(monitor.size.0.saturating_sub(size.0) / 2).unwrap_or(0),
        monitor.position.1 + i32::try_from(monitor.size.1.saturating_sub(size.1) / 2).unwrap_or(0),
    );
    let position = match record.position {
        Some((x, y)) if named.is_some() && visible_on(monitor, (x, y), size) => Some((x, y)),
        // No position to keep (Wayland) on the same monitor: let the window system place it.
        None if named.is_some() => None,
        _ => Some(centered),
    };
    Placement {
        monitor: index,
        position,
        size,
        fullscreen: record.fullscreen,
        maximized: record.maximized,
    }
}

/// Whether at least [`MIN_VISIBLE`] pixels of each axis of a window at `pos` lie on `monitor`.
fn visible_on(monitor: &MonitorInfo, pos: (i32, i32), size: (u32, u32)) -> bool {
    let span = |start: i32, len: u32, m_start: i32, m_len: u32| {
        let end = i64::from(start) + i64::from(len);
        let m_end = i64::from(m_start) + i64::from(m_len);
        end.min(m_end) - i64::from(start.max(m_start)) >= i64::from(MIN_VISIBLE)
    };
    span(pos.0, size.0, monitor.position.0, monitor.size.0)
        && span(pos.1, size.1, monitor.position.1, monitor.size.1)
}

/// Asset documents waiting to reopen, one per frame through the Content Browser's open route.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct ReopenQueue {
    /// Absolute paths, in order.
    pub pending: Vec<PathBuf>,
}

/// The next asset document to reopen, if any.
pub fn next_reopen(world: &mut World) -> Option<PathBuf> {
    // Read first: an empty queue is not marked changed every frame.
    if world
        .get_resource::<ReopenQueue>()
        .is_none_or(|queue| queue.pending.is_empty())
    {
        return None;
    }
    let mut queue = world.get_resource_mut::<ReopenQueue>()?;
    Some(queue.pending.remove(0))
}

/// `path` relative to `root` with `/` separators (`None` outside the project).
#[must_use]
pub fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The record to save for the project at `root`: the observed window (or a default size), the
/// open level, and `assets` (absolute paths of the documents open in asset editors).
#[must_use]
pub fn capture(world: &World, root: &Path, assets: &[PathBuf]) -> WindowRecord {
    let observed = world.get_resource::<ObservedWindow>();
    let level = world
        .get_resource::<crate::level_io::OpenLevel>()
        .and_then(|open| open.path.as_deref())
        .and_then(|path| relative(root, path));
    let mut asset_tabs: Vec<String> = Vec::new();
    for rel in assets.iter().filter_map(|p| relative(root, p)) {
        if !asset_tabs.contains(&rel) {
            asset_tabs.push(rel);
        }
    }
    WindowRecord {
        monitor: observed.and_then(|o| o.monitor.clone()),
        position: observed.and_then(|o| o.position),
        size: observed.map_or((1600, 900), |o| o.size),
        fullscreen: observed.is_some_and(|o| o.fullscreen),
        maximized: observed.is_some_and(|o| o.maximized),
        level,
        asset_tabs,
    }
}

/// Reopens project `root`'s saved level (when the editor still shows a new, unsaved level) and
/// queues its asset documents. Returns the status line, `None` when nothing was saved.
pub fn reopen_documents(world: &mut World, config_dir: &Path, root: &Path) -> Option<String> {
    let record = read_record(config_dir, root)?;
    let mut lines = Vec::new();
    let fresh = world
        .get_resource::<crate::level_io::OpenLevel>()
        .is_none_or(|open| open.path.is_none());
    if fresh
        && let Some(level) = &record.level
        && !world
            .get_resource::<crate::command::UndoStack>()
            .is_some_and(crate::command::UndoStack::is_dirty)
    {
        let path = root.join(level);
        if path.exists() {
            match crate::level_io::open_level_file(world, &path) {
                Ok(open) => {
                    world.insert_resource(open);
                    lines.push(format!("reopened {level}"));
                }
                Err(err) => lines.push(format!("{level} not reopened: {err}")),
            }
        }
    }
    let assets: Vec<PathBuf> = record
        .asset_tabs
        .iter()
        .map(|rel| root.join(rel))
        .filter(|p| p.exists())
        .collect();
    if !assets.is_empty() {
        lines.push(format!("reopening {} asset document(s)", assets.len()));
        world
            .get_resource_or_insert_with(ReopenQueue::default)
            .pending
            .extend(assets);
    }
    (!lines.is_empty()).then(|| lines.join("; "))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn monitor(name: &str, x: i32, w: u32, primary: bool) -> MonitorInfo {
        MonitorInfo {
            name: Some(name.to_owned()),
            position: (x, 0),
            size: (w, 1080),
            primary,
        }
    }

    fn record(monitor: Option<&str>, position: Option<(i32, i32)>) -> WindowRecord {
        WindowRecord {
            monitor: monitor.map(str::to_owned),
            position,
            size: (1280, 720),
            fullscreen: false,
            maximized: true,
            level: None,
            asset_tabs: Vec::new(),
        }
    }

    #[test]
    fn a_missing_monitor_falls_back_to_the_primary_display() {
        let monitors = [monitor("left", 0, 1920, false), monitor("main", 1920, 2560, true)];
        // On its monitor: kept.
        let kept = place(&record(Some("left"), Some((100, 50))), &monitors);
        assert_eq!(
            (kept.monitor, kept.position, kept.size),
            (Some(0), Some((100, 50)), (1280, 720))
        );
        assert!(kept.maximized);
        // Its monitor is gone: centered on the primary one.
        let moved = place(&record(Some("gone"), Some((100, 50))), &monitors);
        assert_eq!(moved.monitor, Some(1));
        assert_eq!(moved.position, Some((1920 + (2560 - 1280) / 2, (1080 - 720) / 2)));
        // Off its monitor: centered on it.
        let off = place(&record(Some("left"), Some((5000, 50))), &monitors);
        assert_eq!(off.position, Some(((1920 - 1280) / 2, (1080 - 720) / 2)));
        // Too big: shrunk to the monitor.
        let mut big = record(Some("left"), Some((0, 0)));
        big.size = (9000, 9000);
        assert_eq!(place(&big, &monitors).size, (1920, 1080));
        // No primary flag and no match: the first monitor. No monitors: as saved.
        let plain = [monitor("a", 0, 1920, false)];
        assert_eq!(place(&record(Some("gone"), None), &plain).monitor, Some(0));
        assert_eq!(
            place(&record(Some("gone"), Some((7, 8))), &[]).position,
            Some((7, 8))
        );
        // Wayland gives no position: the window system places it on the same monitor.
        assert_eq!(place(&record(Some("left"), None), &monitors).position, None);
    }

    #[test]
    fn the_file_round_trips_keeps_newest_projects_and_refuses_newer_files() {
        let mut file = WindowStateFile::default();
        for i in 0..MAX_PROJECTS + 3 {
            file.put(&format!("/p/{i}"), record(None, None));
        }
        assert_eq!(file.projects.len(), MAX_PROJECTS);
        assert!(!file.projects.contains_key("/p/0"));
        assert!(file.projects.contains_key(&format!("/p/{}", MAX_PROJECTS + 2)));
        let back = WindowStateFile::from_text(&file.to_text()).unwrap();
        assert_eq!(back, file);
        let newer = file.to_text().replace("format_version: 1", "format_version: 5");
        assert!(WindowStateFile::from_text(&newer).is_err());
        assert_eq!(
            relative(Path::new("/p"), Path::new("/p/a/b.ron")).as_deref(),
            Some("a/b.ron")
        );
        assert_eq!(relative(Path::new("/p"), Path::new("/q/a.ron")), None);
    }
}
