// SPDX-License-Identifier: MPL-2.0
//! The editor's view state per project (`<project>/.regia/editor_view.ron`, ADR-7275): File ▸
//! Recent, and per level the perspective camera, the camera bookmarks, the Outliner locks and the
//! layer locks (ADR-7792).
//!
//! The editor-viewport lane (ADR-7204) kept all of these for the session only. [`tick`] (in `Last`)
//! now loads the file once per project and seeds [`RecentLevels`] from it; when a level opens it
//! restores that level's camera, bookmarks and locks; while the level is open it records them. The
//! file is written when the state changed and [`WRITE_EVERY_FRAMES`] frames passed since the last
//! write, when another level opens, and on exit ([`flush`]). It is editor-only state: nothing in
//! the level file changes, and a lock is stored by the entity's `EntityGuid` (entities without one
//! are not saved).
//!
//! ```ron
//! (
//!     format_version: 1,
//!     recent_levels: ["assets/maps/room.level.ron"],
//!     levels: {
//!         "assets/maps/room.level.ron": (
//!             camera: Some((target: (0.0, 1.0, 0.0), distance: 8.0, yaw: 0.0, pitch: 1.5, speed: 6.0, fov_deg: 60.0)),
//!             bookmarks: { 3: (target: (0.0, 1.0, 0.0), distance: 8.0, yaw: 0.0, pitch: 0.0, speed: 6.0, fov_deg: 60.0) },
//!             locked: ["67e55044-10b1-426f-9247-bb680e5fe0c8"],
//!             locked_layers: ["night_props"],
//!         ),
//!     },
//! )
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_scene::components::EntityGuid;
use serde::{Deserialize, Serialize};

use crate::selection::{SelectMode, Selection};
use crate::viewport::EditorCamera;
use crate::viewport_ops::{CameraEases, EditorLocked, MAIN_CAMERA, RecentLevels};
use crate::views::CameraBookmarks;

/// The view state file, relative to the project root.
pub const VIEW_STATE_FILE: &str = ".regia/editor_view.ron";

/// Current version of `editor_view.ron`.
pub const VIEW_STATE_FORMAT_VERSION: u32 = 1;

/// Frames between two writes of a changed state (orbiting the camera does not write every frame).
pub const WRITE_EVERY_FRAMES: u64 = 60;

/// Levels the file keeps; past it, only the recent levels and the open one stay.
pub const MAX_LEVELS: usize = 64;

/// Frames a lock waits for its entity to appear after a level opens (levels that spawn late).
const PENDING_LOCK_FRAMES: u32 = 30;

/// A perspective camera pose ([`EditorCamera`]) as the file stores it.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct CameraPose {
    /// Orbit target, world space.
    pub target: [f32; 3],
    /// Distance from the target, meters.
    pub distance: f32,
    /// Yaw around +Y, radians.
    pub yaw: f32,
    /// Pitch above the horizon, radians.
    pub pitch: f32,
    /// Fly speed, meters per second.
    pub speed: f32,
    /// Vertical field of view, degrees.
    pub fov_deg: f32,
}

impl CameraPose {
    /// The pose of `camera`.
    #[must_use]
    pub fn of(camera: &EditorCamera) -> Self {
        Self {
            target: camera.target.to_array(),
            distance: camera.distance,
            yaw: camera.yaw,
            pitch: camera.pitch,
            speed: camera.speed,
            fov_deg: camera.fov_deg,
        }
    }

    /// The camera of this pose; `None` when a value is not finite, the distance or speed is not
    /// positive, or the field of view is outside 1..179 degrees (a hand-edited file).
    #[must_use]
    pub fn camera(&self) -> Option<EditorCamera> {
        let finite = self.target.iter().all(|v| v.is_finite())
            && [self.distance, self.yaw, self.pitch, self.speed, self.fov_deg]
                .iter()
                .all(|v| v.is_finite());
        if !finite || self.distance <= 0.0 || self.speed <= 0.0 || !(1.0..=179.0).contains(&self.fov_deg) {
            return None;
        }
        Some(EditorCamera {
            target: Vec3::from_array(self.target),
            distance: self.distance,
            yaw: self.yaw,
            pitch: self.pitch,
            speed: self.speed,
            fov_deg: self.fov_deg,
            // Clip planes are session state (ADR-7755): not in the saved pose.
            ..EditorCamera::default()
        })
    }
}

/// The view state of one level.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct LevelView {
    /// The perspective camera when the level was last open.
    pub camera: Option<CameraPose>,
    /// Camera bookmarks by slot (0-9).
    pub bookmarks: BTreeMap<u8, CameraPose>,
    /// The `EntityGuid`s of the locked entities, sorted.
    pub locked: Vec<Guid>,
    /// The `EntityGuid`s of the entities hidden in the editor only (H, isolate; ADR-7770), sorted.
    pub hidden: Vec<Guid>,
    /// The `EntityGuid`s an isolate hid (Shift+H restores them), sorted.
    pub isolated: Vec<Guid>,
    /// The locked layers, sorted (ADR-7792; left out of the file when empty).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub locked_layers: Vec<String>,
    /// The 2D view's guides (ADR-7853); not written when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub guides_2d: Vec<Guide2dPose>,
}

/// One 2D guide as the file stores it: `vertical` (else horizontal) at `at` level units.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Guide2dPose {
    /// A vertical guide (at an x), else horizontal (at a y).
    pub vertical: bool,
    /// Its x or y, level units.
    pub at: f32,
}

/// `editor_view.ron`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ViewStateDoc {
    /// [`VIEW_STATE_FORMAT_VERSION`] when written.
    pub format_version: u32,
    /// File ▸ Recent, newest first, project-relative with `/` separators.
    pub recent_levels: Vec<String>,
    /// Per level (project-relative path), its view state.
    pub levels: BTreeMap<String, LevelView>,
    /// The UI designer's preview size, safe-area insets and theme, as last set (a document opened
    /// next session starts with them; ADR-7677, ADR-7744).
    pub ui_designer: Option<crate::ui_designer_preview::PreviewView>,
}

impl Default for ViewStateDoc {
    fn default() -> Self {
        Self {
            format_version: VIEW_STATE_FORMAT_VERSION,
            recent_levels: Vec::new(),
            levels: BTreeMap::new(),
            ui_designer: None,
        }
    }
}

/// The `from` versions of the registered `editor_view.ron` migration steps (none: version 1 is the
/// first).
#[must_use]
pub fn view_state_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde shape of `.regia/editor_view.ron` (ADR-7902: the `editor-view-state` format).
#[must_use]
pub fn view_state_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<ViewStateDoc>(),
        regia_core::schema::serde_field_names::<LevelView>(),
        regia_core::schema::serde_field_names::<CameraPose>(),
        regia_core::schema::serde_field_names::<crate::ui_designer_preview::PreviewView>(),
    ])
}

/// Why `editor_view.ron` was not read or written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewStateError {
    /// The file is not valid RON of this shape.
    Parse(String),
    /// Written by a newer editor; left untouched (never overwritten).
    Newer {
        /// The file's `format_version`.
        found: u32,
    },
    /// Reading or writing failed.
    Io(String),
}

impl ViewStateError {
    /// The stable error code (`view_state_parse`, `view_state_newer`, `view_state_io`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "view_state_parse",
            Self::Newer { .. } => "view_state_newer",
            Self::Io(_) => "view_state_io",
        }
    }
}

impl std::fmt::Display for ViewStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "{VIEW_STATE_FILE} is not valid: {e}"),
            Self::Newer { found } => write!(
                f,
                "{VIEW_STATE_FILE} was written by a newer editor (format_version {found}, this editor reads up to \
                 {VIEW_STATE_FORMAT_VERSION}); left untouched"
            ),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ViewStateError {}

impl ViewStateDoc {
    /// Parses the file's text. A version-0 or missing `format_version` reads as the current one.
    ///
    /// # Errors
    ///
    /// [`ViewStateError::Parse`] for text that is not this shape, [`ViewStateError::Newer`] for a
    /// newer `format_version`.
    pub fn from_text(text: &str) -> Result<Self, ViewStateError> {
        let mut doc: Self = ron::from_str(text).map_err(|e| ViewStateError::Parse(e.to_string()))?;
        if doc.format_version > VIEW_STATE_FORMAT_VERSION {
            return Err(ViewStateError::Newer {
                found: doc.format_version,
            });
        }
        doc.format_version = VIEW_STATE_FORMAT_VERSION;
        Ok(doc)
    }

    /// The file's text at the current version.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut doc = self.clone();
        doc.format_version = VIEW_STATE_FORMAT_VERSION;
        ron::ser::to_string_pretty(&doc, regia_core::ron_pretty()).unwrap_or_default()
    }
}

/// `<root>/.regia/editor_view.ron`.
#[must_use]
pub fn view_state_path(root: &Path) -> PathBuf {
    root.join(VIEW_STATE_FILE)
}

/// Reads the file; `Ok(None)` when it does not exist.
///
/// # Errors
///
/// As [`ViewStateDoc::from_text`], or [`ViewStateError::Io`] when it cannot be read.
pub fn read_view_state(path: &Path) -> Result<Option<ViewStateDoc>, ViewStateError> {
    match std::fs::read_to_string(path) {
        Ok(text) => ViewStateDoc::from_text(&text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ViewStateError::Io(format!("cannot read {}: {e}", path.display()))),
    }
}

/// Writes the file atomically.
///
/// # Errors
///
/// [`ViewStateError::Io`] when the directory or file cannot be written.
pub fn write_view_state(path: &Path, doc: &ViewStateDoc) -> Result<(), ViewStateError> {
    crate::atomic_write::write_atomic(path, doc.to_text().as_bytes()).map_err(ViewStateError::Io)
}

/// The key of a level path: project-relative with `/` separators; `None` outside the project.
#[must_use]
pub fn level_key(root: &Path, path: &Path) -> Option<String> {
    let rel = if path.is_absolute() {
        path.strip_prefix(root).ok()?.to_path_buf()
    } else {
        path.to_path_buf()
    };
    let parts: Vec<String> = rel
        .components()
        .map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Option<_>>()?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The editor's view-state bookkeeping (one per edit world).
#[derive(Resource, Debug, Default)]
pub struct ViewStateTracker {
    /// The project whose file [`Self::doc`] mirrors.
    root: Option<PathBuf>,
    /// The state as the editor sees it now.
    pub doc: ViewStateDoc,
    /// What the file holds (last read or written).
    written: Option<ViewStateDoc>,
    /// The open level's key, once its state was restored.
    level: Option<String>,
    /// Locks of the open level whose entity has not appeared yet, and frames left to wait.
    pending_locks: BTreeSet<Guid>,
    /// Editor-only hides (and the isolate's share of them) not applied yet (ADR-7770).
    pending_hidden: (BTreeSet<Guid>, BTreeSet<Guid>),
    pending_frames: u32,
    frames_since_write: u64,
    /// Never write: the file is from a newer editor or could not be parsed.
    read_only: bool,
    /// The last read or write error.
    pub last_error: Option<ViewStateError>,
    /// Writes so far (for tests and the agent surface).
    pub writes: u64,
}

impl ViewStateTracker {
    /// The project root of the loaded file.
    #[must_use]
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// The open level's key, once its state was restored.
    #[must_use]
    pub fn level(&self) -> Option<&str> {
        self.level.as_deref()
    }

    /// Whether the state differs from the file.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.written.as_ref() != Some(&self.doc)
    }

    /// Whether the file is never written (newer version or not parsable).
    #[must_use]
    pub fn read_only(&self) -> bool {
        self.read_only
    }
}

fn project_root(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
}

fn exiting(world: &World) -> bool {
    world
        .get_resource::<crate::headless::HeadlessEditor>()
        .is_some_and(|h| h.shutdown_requested)
        || world.contains_resource::<crate::headless::ShutdownRequest>()
        || world
            .get_resource::<crate::quit::EditorQuit>()
            .is_some_and(crate::quit::EditorQuit::is_requested)
}

/// Loads the project's file into the tracker (once per project) and seeds [`RecentLevels`].
fn ensure_loaded(world: &mut World, tracker: &mut ViewStateTracker, root: &Path) {
    if tracker.root.as_deref() == Some(root) {
        return;
    }
    *tracker = ViewStateTracker {
        root: Some(root.to_path_buf()),
        writes: tracker.writes,
        ..ViewStateTracker::default()
    };
    match read_view_state(&view_state_path(root)) {
        Ok(Some(doc)) => {
            tracker.written = Some(doc.clone());
            tracker.doc = doc;
        }
        Ok(None) => tracker.written = Some(ViewStateDoc::default()),
        Err(err) => {
            tracing::warn!(target: "regia_editor", "{err}");
            tracker.read_only = !matches!(err, ViewStateError::Io(_));
            tracker.last_error = Some(err);
        }
    }
    // The UI designer's remembered preview view: a document already open takes it too.
    if let Some(view) = tracker.doc.ui_designer.clone() {
        let mut designer = world.get_resource_or_insert_with(crate::ui_designer::UiDesigner::default);
        designer.preview_view = Some(view);
        designer.apply_preview_view();
    }
    let mut recent = world.get_resource_or_insert_with(RecentLevels::default);
    for rel in &tracker.doc.recent_levels {
        let path = root.join(rel);
        if recent.paths.len() < RecentLevels::CAP && !recent.paths.contains(&path) && path.exists() {
            recent.paths.push(path);
        }
    }
}

/// The open level's state as the world holds it.
fn capture(
    world: &mut World,
    pending: &BTreeSet<Guid>,
    pending_hidden: &(BTreeSet<Guid>, BTreeSet<Guid>),
) -> LevelView {
    let camera = world.get_resource::<EditorCamera>().map(CameraPose::of);
    let bookmarks = world
        .get_resource::<CameraBookmarks>()
        .map(|marks| {
            (0u8..10)
                .filter_map(|slot| marks.get(usize::from(slot)).map(|c| (slot, CameraPose::of(c))))
                .collect()
        })
        .unwrap_or_default();
    let mut locked: BTreeSet<Guid> = pending.clone();
    let mut query = world.query_filtered::<&EntityGuid, With<EditorLocked>>();
    locked.extend(query.iter(world).map(|g| g.0));
    let (mut hidden, mut isolated) = pending_hidden.clone();
    let mut query = world.query_filtered::<(Entity, &EntityGuid), With<ViewHidden>>();
    let isolate = world.get_resource::<ViewIsolate>().cloned().unwrap_or_default();
    for (entity, guid) in query.iter(world) {
        hidden.insert(guid.0);
        if isolate.hidden.contains(&entity) {
            isolated.insert(guid.0);
        }
    }
    let locked_layers = world
        .get_resource::<crate::layers_panel::LayerLocks>()
        .map(|locks| locks.locked.iter().cloned().collect())
        .unwrap_or_default();
    let guides_2d = crate::mode2d::guides::saved_guides(world);
    LevelView {
        camera,
        bookmarks,
        locked: locked.into_iter().collect(),
        hidden: hidden.into_iter().collect(),
        isolated: isolated.into_iter().collect(),
        locked_layers,
        guides_2d,
    }
}

/// Locks the entities of `guids` that exist; returns the ones not found.
fn apply_locks(world: &mut World, guids: &BTreeSet<Guid>) -> BTreeSet<Guid> {
    if guids.is_empty() {
        return BTreeSet::new();
    }
    let mut query = world.query::<(Entity, &EntityGuid)>();
    let found: Vec<(Entity, Guid)> = query
        .iter(world)
        .filter(|(_, g)| guids.contains(&g.0))
        .map(|(e, g)| (e, g.0))
        .collect();
    let mut left = guids.clone();
    for (entity, guid) in found {
        crate::viewport_ops::set_locked(world, entity, true);
        left.remove(&guid);
    }
    left
}

/// Restores `view` into the world: the camera (any eased move stops), the bookmarks, the locks.
fn restore(world: &mut World, view: &LevelView) -> BTreeSet<Guid> {
    if let Some(mut camera) = view.camera.as_ref().and_then(CameraPose::camera) {
        // The session's clip planes stay (ADR-7755): the saved pose has none.
        if let Some(current) = world.get_resource::<EditorCamera>() {
            (camera.near, camera.far) = (current.near, current.far);
        }
        if let Some(mut eases) = world.get_resource_mut::<CameraEases>() {
            eases.moves.remove(&MAIN_CAMERA);
        }
        world.insert_resource(camera);
    }
    let mut marks = CameraBookmarks::default();
    for (slot, pose) in &view.bookmarks {
        if let Some(camera) = pose.camera() {
            marks.set(usize::from(*slot), camera);
        }
    }
    world.insert_resource(marks);
    // The level's layer locks replace the previous level's (ADR-7792).
    world.insert_resource(crate::layers_panel::LayerLocks {
        locked: view.locked_layers.iter().cloned().collect(),
    });
    crate::mode2d::guides::restore_guides(world, &view.guides_2d);
    apply_locks(world, &view.locked.iter().copied().collect())
}

/// Hides (editor only) the entities of `hidden` that exist, recording the ones in `isolated` as the
/// isolate's; returns the ones not found (ADR-7770).
fn apply_hidden(
    world: &mut World,
    (hidden, isolated): &(BTreeSet<Guid>, BTreeSet<Guid>),
) -> (BTreeSet<Guid>, BTreeSet<Guid>) {
    if hidden.is_empty() {
        return Default::default();
    }
    let mut query = world.query::<(Entity, &EntityGuid)>();
    let found: Vec<(Entity, Guid)> = query
        .iter(world)
        .filter(|(_, g)| hidden.contains(&g.0))
        .map(|(e, g)| (e, g.0))
        .collect();
    let (mut left_hidden, mut left_isolated) = (hidden.clone(), isolated.clone());
    for (entity, guid) in found {
        world.entity_mut(entity).insert(ViewHidden);
        if left_isolated.remove(&guid) {
            world
                .get_resource_or_insert_with(ViewIsolate::default)
                .hidden
                .insert(entity);
        }
        left_hidden.remove(&guid);
    }
    (left_hidden, left_isolated)
}

/// `Last`: loads, restores, records and writes the view state (see the module docs). Does
/// nothing without a project.
pub fn tick(world: &mut World) {
    let _span = tracing::trace_span!(target: "regia_editor", "view_state_tick").entered();
    let Some(root) = project_root(world) else {
        return;
    };
    let mut tracker = world.remove_resource::<ViewStateTracker>().unwrap_or_default();
    ensure_loaded(world, &mut tracker, &root);
    // File ▸ Recent follows the open level in the headless editor too (the shell's menu bar does
    // it in the windowed one; the call is idempotent).
    crate::viewport_ops::note_recent_level(world);
    let key = world
        .get_resource::<crate::level_io::OpenLevel>()
        .and_then(|open| open.path.clone())
        .and_then(|path| level_key(&root, &path));
    let opened = key != tracker.level;
    if opened {
        tracker.level.clone_from(&key);
        tracker.pending_locks.clear();
        tracker.pending_hidden = Default::default();
        // A level without a saved state keeps the session's camera and bookmarks (an untitled
        // level saved under a name keeps what it had).
        if let Some(view) = key.as_ref().and_then(|k| tracker.doc.levels.get(k)).cloned() {
            tracker.pending_locks = restore(world, &view);
            tracker.pending_hidden = apply_hidden(
                world,
                &(
                    view.hidden.iter().copied().collect(),
                    view.isolated.iter().copied().collect(),
                ),
            );
            tracker.pending_frames = PENDING_LOCK_FRAMES;
        }
    } else if !tracker.pending_locks.is_empty() || !tracker.pending_hidden.0.is_empty() {
        let pending = std::mem::take(&mut tracker.pending_locks);
        tracker.pending_locks = apply_locks(world, &pending);
        let pending = std::mem::take(&mut tracker.pending_hidden);
        tracker.pending_hidden = apply_hidden(world, &pending);
        tracker.pending_frames = tracker.pending_frames.saturating_sub(1);
        if tracker.pending_frames == 0 {
            tracker.pending_locks.clear();
            tracker.pending_hidden = Default::default();
        }
    }
    if let Some(key) = &key {
        let view = capture(world, &tracker.pending_locks, &tracker.pending_hidden);
        if tracker.doc.levels.get(key) != Some(&view) {
            tracker.doc.levels.insert(key.clone(), view);
        }
    }
    // The UI designer's preview size, insets and theme follow the open document.
    if let Some(mut designer) = world.get_resource_mut::<crate::ui_designer::UiDesigner>()
        && let Some(view) = designer.doc().map(|d| d.preview.view())
        && designer.preview_view.as_ref() != Some(&view)
    {
        designer.preview_view = Some(view.clone());
        tracker.doc.ui_designer = Some(view);
    }
    let recent: Vec<String> = world
        .get_resource::<RecentLevels>()
        .map(|r| r.paths.iter().filter_map(|p| level_key(&root, p)).collect())
        .unwrap_or_default();
    if tracker.doc.recent_levels != recent {
        tracker.doc.recent_levels = recent;
    }
    if tracker.doc.levels.len() > MAX_LEVELS {
        let keep: BTreeSet<String> = tracker
            .doc
            .recent_levels
            .iter()
            .cloned()
            .chain(key.clone())
            .collect();
        tracker.doc.levels.retain(|k, _| keep.contains(k));
    }
    tracker.frames_since_write = tracker.frames_since_write.saturating_add(1);
    let due = opened || exiting(world) || tracker.frames_since_write >= WRITE_EVERY_FRAMES;
    if due && tracker.dirty() {
        write_now(&mut tracker, &root);
    }
    world.insert_resource(tracker);
}

fn write_now(tracker: &mut ViewStateTracker, root: &Path) {
    if tracker.read_only {
        return;
    }
    tracker.frames_since_write = 0;
    match write_view_state(&view_state_path(root), &tracker.doc) {
        Ok(()) => {
            tracker.written = Some(tracker.doc.clone());
            tracker.writes += 1;
            tracker.last_error = None;
        }
        Err(err) => {
            tracing::warn!(target: "regia_editor", "{err}");
            tracker.last_error = Some(err);
        }
    }
}

/// Records the current state and writes the file now when it changed (the editor's exit paths
/// call it; a no-op without a project).
pub fn flush(world: &mut World) {
    let Some(root) = project_root(world) else {
        return;
    };
    tick(world);
    if let Some(mut tracker) = world.remove_resource::<ViewStateTracker>() {
        if tracker.dirty() {
            write_now(&mut tracker, &root);
        }
        world.insert_resource(tracker);
    }
}

// ---------------------------------------------------------------------------------------------
// Agent surface.

fn pose_json(pose: &CameraPose) -> serde_json::Value {
    serde_json::json!({
        "target": pose.target,
        "distance": pose.distance,
        "yaw": pose.yaw,
        "pitch": pose.pitch,
        "fov_deg": pose.fov_deg,
    })
}

/// `editor.view_state`: what the editor keeps in `editor_view.ron` (read only).
fn view_state(
    world: &mut World,
    _params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    use serde_json::json;
    let Some(root) = project_root(world) else {
        return Err(regia_agent::protocol::RpcError::new(
            regia_agent::protocol::NOT_FOUND,
            "view_state_no_project",
            "the editor has no project",
            "start the editor with --project",
        ));
    };
    let tracker = world.get_resource::<ViewStateTracker>();
    let doc = tracker.map(|t| t.doc.clone()).unwrap_or_default();
    let levels: serde_json::Map<String, serde_json::Value> = doc
        .levels
        .iter()
        .map(|(key, view)| {
            (
                key.clone(),
                json!({
                    "camera": view.camera.as_ref().map(pose_json),
                    "bookmarks": view
                        .bookmarks
                        .iter()
                        .map(|(slot, pose)| json!({ "slot": slot, "camera": pose_json(pose) }))
                        .collect::<Vec<_>>(),
                    "locked": view.locked.iter().map(ToString::to_string).collect::<Vec<_>>(),
                    "hidden": view.hidden.iter().map(ToString::to_string).collect::<Vec<_>>(),
                    "isolated": view.isolated.iter().map(ToString::to_string).collect::<Vec<_>>(),
                }),
            )
        })
        .collect();
    Ok(json!({
        "file": VIEW_STATE_FILE,
        "exists": view_state_path(&root).exists(),
        "level": tracker.and_then(ViewStateTracker::level),
        "recent": doc.recent_levels,
        "levels": levels,
        "dirty": tracker.is_some_and(ViewStateTracker::dirty),
        "read_only": tracker.is_some_and(ViewStateTracker::read_only),
        "writes": tracker.map_or(0, |t| t.writes),
        "error": tracker.and_then(|t| t.last_error.as_ref()).map(|e| json!({ "code": e.code(), "message": e.to_string() })),
    }))
}

/// The methods of this module (`editor.view_state`).
#[must_use]
pub fn methods() -> Vec<(regia_agent::schema::MethodSchema, crate::viewport_agent::Handler)> {
    use regia_agent::schema::{MethodSchema, Permission};
    vec![(
        MethodSchema {
            name: "editor.view_state",
            tool: "view_state",
            description: "The view state the editor keeps per project in .regia/editor_view.ron (ADR-7275): File \
                Recent, and per level the perspective camera, the camera bookmarks and the locked entities \
                (by EntityGuid). Restored when a level opens; written when it changed (at most every 60 \
                frames), when another level opens and on exit.",
            params: &[],
            result: "{ file, exists, level, recent, levels: { <path>: { camera, bookmarks: [{slot, camera}], locked: [guid], hidden: [guid], isolated: [guid] } }, dirty, read_only, writes, error }",
            mutating: false,
            permission: Permission::Read,
        },
        view_state as crate::viewport_agent::Handler,
    )]
    .into_iter()
    .chain(hide_methods())
    .collect()
}

// ---------------------------------------------------------------------------------------------
// Editor-only hide and isolate (ADR-7770).

/// Hidden in the editor only (H, Shift+H isolate): the entity and its children do not draw in the
/// viewport and cannot be picked. Per user: kept in `editor_view.ron` by `EntityGuid`, never in the
/// level, never an undo step, never dirtying. The Outliner eye (the saved
/// `Visibility::Hidden`) is separate and shows this state as a dimmed eye.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ViewHidden;

/// The entities the current isolate hid (Shift+H again shows them).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewIsolate {
    /// Hidden by the isolate.
    pub hidden: BTreeSet<Entity>,
}

/// Why an isolate did not start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolateError {
    /// Nothing is selected.
    EmptySelection,
}

impl IsolateError {
    /// The stable error code (`isolate_empty_selection`).
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::EmptySelection => "isolate_empty_selection",
        }
    }
}

/// Whether `entity`, its pick owner (the model a part draws for) or an ancestor is hidden in the
/// editor only.
#[must_use]
pub fn is_view_hidden(world: &World, entity: Entity) -> bool {
    let owner = crate::mesh_source::pick_owner(world, entity);
    [entity, owner]
        .into_iter()
        .chain(crate::hierarchy_ops::ancestors(world, owner))
        .any(|e| world.get::<ViewHidden>(e).is_some())
}

/// Whether `entity` itself carries the editor-only hide (the Outliner's dimmed eye).
#[must_use]
pub fn has_view_hidden(world: &World, entity: Entity) -> bool {
    world.get::<ViewHidden>(entity).is_some()
}

/// Whether an isolate is active.
#[must_use]
pub fn isolating(world: &World) -> bool {
    world
        .get_resource::<ViewIsolate>()
        .is_some_and(|isolate| !isolate.hidden.is_empty())
}

/// Hides (`hidden`) or shows `entities` in the editor only; hidden entities leave the selection.
/// Returns how many changed. Not an undo step; the level stays clean.
pub fn set_view_hidden(world: &mut World, entities: &[Entity], hidden: bool) -> usize {
    let mut changed = 0;
    for &entity in entities {
        if world.get_entity(entity).is_err() || has_view_hidden(world, entity) == hidden {
            continue;
        }
        if hidden {
            world.entity_mut(entity).insert(ViewHidden);
        } else {
            world.entity_mut(entity).remove::<ViewHidden>();
            if let Some(mut isolate) = world.get_resource_mut::<ViewIsolate>() {
                isolate.hidden.remove(&entity);
            }
        }
        changed += 1;
    }
    if hidden && changed > 0 {
        let mut selection = world.get_resource_or_insert_with(Selection::default).clone();
        let kept: Vec<Entity> = selection
            .entities()
            .iter()
            .copied()
            .filter(|e| !is_view_hidden(world, *e))
            .collect();
        selection.clear();
        for e in kept {
            selection.select(e, SelectMode::Add);
        }
        world.insert_resource(selection);
    }
    changed
}

/// Shows every entity hidden in the editor only (Alt+H) and ends any isolate. Returns how many.
pub fn unhide_all(world: &mut World) -> usize {
    let mut query = world.query_filtered::<Entity, With<ViewHidden>>();
    let hidden: Vec<Entity> = query.iter(world).collect();
    for &entity in &hidden {
        world.entity_mut(entity).remove::<ViewHidden>();
    }
    world.remove_resource::<ViewIsolate>();
    hidden.len()
}

/// Starts (`on`) or ends an isolate (Shift+H): starting hides, in the editor only, every user
/// entity that is not selected, not under a selected entity and not an ancestor of one; ending
/// shows what the isolate hid. Returns how many entities changed (0 when the isolate already was
/// in that state).
///
/// # Errors
///
/// [`IsolateError::EmptySelection`] when starting with nothing selected.
pub fn set_isolate(world: &mut World, on: bool) -> Result<usize, IsolateError> {
    if !on {
        let Some(isolate) = world.remove_resource::<ViewIsolate>() else {
            return Ok(0);
        };
        let entities: Vec<Entity> = isolate.hidden.into_iter().collect();
        return Ok(set_view_hidden(world, &entities, false));
    }
    if isolating(world) {
        return Ok(0);
    }
    let selected: BTreeSet<Entity> = world
        .get_resource::<Selection>()
        .map(|s| s.entities().iter().copied().collect())
        .unwrap_or_default();
    if selected.is_empty() {
        return Err(IsolateError::EmptySelection);
    }
    let mut keep: BTreeSet<Entity> = BTreeSet::new();
    for &entity in &selected {
        let owner = crate::mesh_source::pick_owner(world, entity);
        keep.insert(owner);
        keep.extend(crate::hierarchy_ops::ancestors(world, owner));
    }
    let candidates: Vec<Entity> = world
        .iter_entities()
        .map(|e| e.id())
        .filter(|&e| {
            crate::visibility::is_user_entity(world, e)
                && !crate::visibility::is_hidden(world, e)
                && !keep.contains(&e)
                && !has_view_hidden(world, e)
        })
        .collect();
    let mut hide = Vec::new();
    for entity in candidates {
        // Under a selected entity (its own parts and children) stays.
        let owner = crate::mesh_source::pick_owner(world, entity);
        let under = std::iter::once(owner)
            .chain(crate::hierarchy_ops::ancestors(world, owner))
            .any(|e| selected.contains(&e));
        if !under {
            hide.push(entity);
        }
    }
    let changed = set_view_hidden(world, &hide, true);
    world.insert_resource(ViewIsolate {
        hidden: hide.into_iter().collect(),
    });
    Ok(changed)
}

/// Shift+H: ends the isolate when one is active, else starts one.
///
/// # Errors
///
/// As [`set_isolate`].
pub fn toggle_isolate(world: &mut World) -> Result<usize, IsolateError> {
    let on = !isolating(world);
    set_isolate(world, on)
}

/// `editor.hide`.
fn hide(
    world: &mut World,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    use serde_json::{Value, json};
    let flag = |name: &str| params.get(name).and_then(Value::as_bool);
    let hidden = flag("hidden").unwrap_or(true);
    let editor_only = flag("editor_only").unwrap_or(true);
    let changed = if flag("all") == Some(true) {
        if hidden || !editor_only {
            return Err(regia_agent::protocol::RpcError::invalid_params(
                "`all` only shows every editor-only hidden entity (hidden: false, editor_only: true)",
            ));
        }
        unhide_all(world)
    } else {
        let entities = crate::hierarchy_agent::entities_param(world, params)?;
        if let Some(missing) = entities.iter().find(|e| world.get_entity(**e).is_err()) {
            return Err(regia_agent::protocol::RpcError::new(
                regia_agent::protocol::NOT_FOUND,
                "hide_no_entity",
                format!("entity {} does not exist", missing.to_bits()),
                "pass ids from editor.outliner or editor.selection",
            ));
        }
        if editor_only {
            set_view_hidden(world, &entities, hidden)
        } else {
            // The saved visibility (the Outliner eye): one undo step per entity, dirties the level.
            entities
                .iter()
                .filter(|e| crate::viewport_ops::set_visible(world, **e, !hidden))
                .count()
        }
    };
    let mut query = world.query_filtered::<Entity, With<ViewHidden>>();
    let mut all: Vec<u64> = query.iter(world).map(Entity::to_bits).collect();
    all.sort_unstable();
    Ok(json!({
        "changed": changed,
        "editor_hidden": all,
        "isolating": isolating(world),
    }))
}

/// `editor.isolate`.
fn isolate(
    world: &mut World,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    use serde_json::{Value, json};
    let result = match params.get("on").and_then(Value::as_bool) {
        Some(on) => set_isolate(world, on),
        None => toggle_isolate(world),
    };
    let changed = result.map_err(|err| {
        regia_agent::protocol::RpcError::new(
            regia_agent::protocol::INVALID_PARAMS,
            err.code(),
            "nothing is selected to isolate",
            "select the entities to keep, then isolate",
        )
    })?;
    Ok(json!({
        "changed": changed,
        "isolating": isolating(world),
        "hidden": world.get_resource::<ViewIsolate>().map_or(0, |i| i.hidden.len()),
    }))
}

/// The hide and isolate methods (`editor.hide`, `editor.isolate`).
fn hide_methods() -> Vec<(regia_agent::schema::MethodSchema, crate::viewport_agent::Handler)> {
    use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
    vec![
        (
            MethodSchema {
                name: "editor.hide",
                tool: "hide",
                description: "Hides or shows entities (default: the selection). editor_only (default true) is the                     viewport's own hide (H, Alt+H): per user, kept in .regia/editor_view.ron, never saved in the                     level, never an undo step, never dirtying; hidden entities do not draw and cannot be picked.                     editor_only false sets the saved visibility instead (the Outliner eye, one undo step). all                     with hidden false shows every editor-only hidden entity and ends an isolate (ADR-7770).",
                params: &[
                    ParamSchema {
                        name: "entities",
                        kind: "array",
                        required: false,
                        description: "Entity ids (default: the selection).",
                    },
                    ParamSchema {
                        name: "hidden",
                        kind: "boolean",
                        required: false,
                        description: "Hide (default true) or show.",
                    },
                    ParamSchema {
                        name: "editor_only",
                        kind: "boolean",
                        required: false,
                        description: "The editor-only hide (default true) or the saved visibility.",
                    },
                    ParamSchema {
                        name: "all",
                        kind: "boolean",
                        required: false,
                        description: "With hidden false: show every editor-only hidden entity (Alt+H).",
                    },
                ],
                result: "{ changed, editor_hidden: [entity], isolating }",
                mutating: true,
                permission: Permission::EditScene,
            },
            hide as crate::viewport_agent::Handler,
        ),
        (
            MethodSchema {
                name: "editor.isolate",
                tool: "isolate",
                description: "Isolates the selection (Shift+H): hides, in the editor only, everything that is not                     selected, under a selected entity or an ancestor of one; on false (or Shift+H again) shows what                     the isolate hid. Without on it toggles. Never dirties the level (ADR-7770). Error                     isolate_empty_selection.",
                params: &[ParamSchema {
                    name: "on",
                    kind: "boolean",
                    required: false,
                    description: "Start (true) or end (false); absent toggles.",
                }],
                result: "{ changed, isolating, hidden }",
                mutating: true,
                permission: Permission::EditScene,
            },
            isolate as crate::viewport_agent::Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pose(yaw: f32) -> CameraPose {
        CameraPose::of(&EditorCamera {
            yaw,
            ..EditorCamera::default()
        })
    }

    #[test]
    fn the_doc_round_trips_and_a_newer_version_is_refused() {
        let mut doc = ViewStateDoc::default();
        doc.recent_levels.push("assets/maps/a.level.ron".into());
        doc.levels.insert(
            "assets/maps/a.level.ron".into(),
            LevelView {
                camera: Some(pose(0.25)),
                bookmarks: BTreeMap::from([(3, pose(1.0))]),
                locked: vec![Guid(7)],
                // ADR-7770: the editor-only hide round-trips too.
                hidden: vec![Guid(8)],
                isolated: vec![Guid(8)],
                locked_layers: vec!["night".into()],
                guides_2d: vec![crate::view_state::Guide2dPose {
                    vertical: true,
                    at: 48.0,
                }],
            },
        );
        let text = doc.to_text();
        // A version-1 file written before ADR-7770 (no `hidden`, `isolated`) still reads.
        let old = text.replace("hidden: [", "_h: [").replace("isolated: [", "_i: [");
        assert!(ViewStateDoc::from_text(&old).is_ok(), "{old}");
        assert!(text.contains("locked_layers"), "{text}");
        assert_eq!(ViewStateDoc::from_text(&text), Ok(doc.clone()), "{text}");
        let newer = text.replace("format_version: 1", "format_version: 9");
        assert_eq!(
            ViewStateDoc::from_text(&newer).map_err(|e| e.code()),
            Err("view_state_newer")
        );
        assert_eq!(
            ViewStateDoc::from_text("(levels: 3)").map_err(|e| e.code()),
            Err("view_state_parse")
        );
        // Missing fields default.
        assert_eq!(ViewStateDoc::from_text("()"), Ok(ViewStateDoc::default()));
    }

    #[test]
    fn a_hand_edited_pose_with_bad_values_is_skipped() {
        let mut bad = pose(0.0);
        bad.distance = 0.0;
        assert!(bad.camera().is_none());
        bad = pose(f32::NAN);
        assert!(bad.camera().is_none());
        bad = pose(0.0);
        bad.fov_deg = 200.0;
        assert!(bad.camera().is_none());
        assert_eq!(pose(0.5).camera().map(|c| c.yaw), Some(0.5));
    }

    /// The UI designer's preview size, insets and theme persist across sessions (ADR-7744): set,
    /// write the view state, and a new world that loads the project restores them, in a document
    /// that was already open and in one opened later.
    #[test]
    fn the_ui_designer_preview_settings_persist_across_sessions() {
        use crate::ui_designer::UiDesigner;
        let root = regia_test_utils::unique_temp_dir("view-state-ui-designer");
        std::fs::create_dir_all(&root).expect("root");
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let mut designer = UiDesigner::default();
        designer.new_document();
        let doc = designer.doc_mut().expect("doc");
        doc.preview
            .set_window(
                [1000, 700],
                regia_platform::SafeArea {
                    left: 10,
                    top: 20,
                    right: 30,
                    bottom: 40,
                },
            )
            .expect("size");
        doc.preview.theme = Some("night".to_owned());
        world.insert_resource(designer);
        flush(&mut world);
        let text = std::fs::read_to_string(view_state_path(&root)).expect("written");
        assert!(text.contains("ui_designer") && text.contains("night"), "{text}");

        // A new session: a document is open before the project loads.
        let mut again = World::new();
        again.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let mut designer = UiDesigner::default();
        designer.new_document();
        assert_eq!(designer.doc().expect("doc").preview.size, [1920.0, 1080.0]);
        again.insert_resource(designer);
        tick(&mut again);
        let designer = again.resource::<UiDesigner>();
        let preview = &designer.doc().expect("doc").preview;
        assert_eq!(preview.size, [1000.0, 700.0]);
        assert_eq!(preview.safe_area, [10.0, 20.0, 30.0, 40.0]);
        assert_eq!(preview.theme.as_deref(), Some("night"));
        // A document opened later starts with them too.
        let mut designer = again.resource_mut::<UiDesigner>();
        designer.new_document();
        let preview = &designer.doc().expect("doc").preview;
        assert_eq!(preview.size, [1000.0, 700.0]);
        assert_eq!(preview.theme.as_deref(), Some("night"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A hand-edited view that no longer validates is ignored, and the file without the field
    /// reads as before.
    #[test]
    fn a_bad_remembered_ui_view_is_ignored_and_old_files_still_read() {
        use crate::ui_designer_preview::{PreviewSettings, PreviewView};
        let mut settings = PreviewSettings::default();
        let before = settings.clone();
        let bad = PreviewView {
            size: [0.0, 10.0],
            safe_area: [0.0; 4],
            theme: None,
        };
        assert!(!settings.apply_view(&bad));
        assert_eq!(settings, before);
        let old = ViewStateDoc::from_text("(format_version: 1, recent_levels: [], levels: {})").expect("old");
        assert_eq!(old.ui_designer, None);
    }

    #[test]
    fn level_keys_are_project_relative_with_forward_slashes() {
        let root = Path::new("/p/game");
        assert_eq!(
            level_key(root, Path::new("/p/game/assets/maps/a.level.ron")).as_deref(),
            Some("assets/maps/a.level.ron")
        );
        assert_eq!(
            level_key(root, Path::new("assets/maps/a.level.ron")).as_deref(),
            Some("assets/maps/a.level.ron")
        );
        assert_eq!(level_key(root, Path::new("/elsewhere/a.level.ron")), None);
        assert_eq!(level_key(root, Path::new("../a.level.ron")), None);
    }

    /// A world with a project at `root` and the level `assets/maps/a.level.ron` open, and three
    /// entities with stable guids: a parent, its child, and a stranger.
    fn hide_world(root: &Path, guids: [Guid; 3]) -> (World, [Entity; 3]) {
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.to_path_buf()),
        });
        world.insert_resource(crate::level_io::OpenLevel {
            id: regia_scene::prelude::LevelId(Guid::new_random()),
            path: Some(root.join("assets/maps/a.level.ron")),
        });
        let mut undo = crate::command::UndoStack::new();
        undo.mark_saved();
        world.insert_resource(undo);
        world.insert_resource(Selection::default());
        let parent = world
            .spawn((EntityGuid(guids[0]), regia_scene::prelude::Transform::default()))
            .id();
        let child = world
            .spawn((
                EntityGuid(guids[1]),
                regia_scene::prelude::Transform::default(),
                ChildOf(parent),
            ))
            .id();
        let other = world
            .spawn((EntityGuid(guids[2]), regia_scene::prelude::Transform::default()))
            .id();
        (world, [parent, child, other])
    }

    /// ADR-7770 (P-085, P-091): H hides in the editor only (a child under a hidden parent too),
    /// never as an undo step and never dirtying; the hide and an isolate survive a restart through
    /// `editor_view.ron`, and Shift+H after the restart still restores what the isolate hid.
    #[test]
    fn editor_hide_and_isolate_never_dirty_and_survive_a_restart() {
        let root = regia_test_utils::unique_temp_dir("view-hide");
        let guids = [Guid::new_random(), Guid::new_random(), Guid::new_random()];
        let (mut world, [parent, child, other]) = hide_world(&root, guids);
        tick(&mut world);
        assert_eq!(set_view_hidden(&mut world, &[parent], true), 1);
        assert!(
            is_view_hidden(&world, child),
            "a child of a hidden parent is hidden"
        );
        assert!(
            !has_view_hidden(&world, child),
            "only the parent carries the hide"
        );
        assert!(!is_view_hidden(&world, other));
        let undo = world.resource::<crate::command::UndoStack>();
        assert!(!undo.is_dirty(), "hiding never dirties the level");
        assert!(undo.undo_descriptions().is_empty(), "hiding is not an undo step");
        assert_eq!(unhide_all(&mut world), 1);
        // Isolate the child: the parent stays (an ancestor), the stranger is hidden.
        assert_eq!(set_isolate(&mut world, true), Err(IsolateError::EmptySelection));
        world
            .resource_mut::<Selection>()
            .select(child, SelectMode::Replace);
        assert_eq!(set_isolate(&mut world, true), Ok(1));
        assert!(isolating(&world) && is_view_hidden(&world, other) && !is_view_hidden(&world, parent));
        assert!(!world.resource::<crate::command::UndoStack>().is_dirty());
        flush(&mut world);
        let text = std::fs::read_to_string(view_state_path(&root)).expect("written");
        assert!(text.contains(&guids[2].to_string()), "{text}");

        // A restart: a new world with the same project, level and guids.
        let (mut world, [parent, _child, other]) = hide_world(&root, guids);
        tick(&mut world);
        assert!(has_view_hidden(&world, other), "the hide survives the restart");
        assert!(!has_view_hidden(&world, parent));
        assert!(isolating(&world), "and so does the isolate");
        assert_eq!(toggle_isolate(&mut world), Ok(1));
        assert!(!has_view_hidden(&world, other) && !isolating(&world));
        assert!(!world.resource::<crate::command::UndoStack>().is_dirty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADR-7770: hiding drops the hidden entities from the selection; showing keeps it as is.
    #[test]
    fn hiding_leaves_the_selection() {
        let root = regia_test_utils::unique_temp_dir("view-hide-sel");
        let (mut world, [parent, child, other]) = hide_world(
            &root,
            [Guid::new_random(), Guid::new_random(), Guid::new_random()],
        );
        {
            let mut selection = world.resource_mut::<Selection>();
            selection.select(child, SelectMode::Replace);
            selection.select(other, SelectMode::Add);
        }
        set_view_hidden(&mut world, &[parent], true);
        assert_eq!(world.resource::<Selection>().entities(), &[other]);
        assert_eq!(set_view_hidden(&mut world, &[parent], true), 0, "already hidden");
        let _ = std::fs::remove_dir_all(&root);
    }
}
