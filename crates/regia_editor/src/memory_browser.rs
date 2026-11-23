// SPDX-License-Identifier: MPL-2.0
//! Memory snapshots and their diff, the Profiler window's Memory tab (`docs/specs/19-editor.md`
//! § 10, ADR-8142).
//!
//! The tab shows the current and peak bytes of every category (engine subsystem): heap live and
//! peak from the `regia_core::memory` ledger, GPU bytes per subsystem from the allocator report and
//! their highest value seen while the editor ran. **Take snapshot** records, under a name, the
//! category rows, the entity count of every archetype and of every component, and the loaded
//! count and bytes of every asset kind; a diff of two snapshots lists the per-category growth
//! (largest first) and, as the object-leak view, the archetypes, components and asset kinds whose
//! counts changed. A snapshot exports as JSON (a write-only report the editor never reads back).
//!
//! In-process sources (the edit world, play) are snapshotted from this editor's world and
//! process; a PIE process or a dev build from the `memory.report` the Profiler last read from it,
//! without the archetype and component rows (the protocol does not carry them).
//!
//! The agent twin is `editor.memory_snapshot { action: take | list | diff | export }`
//! (`book/agents/profiler.md`).

use std::collections::BTreeMap;
use std::path::{Component as PathComponent, Path, PathBuf};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::ecs::component::ComponentId;
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::profiler_source::{ProfilerSource, ProfilerView};
use crate::viewport_agent::Handler;

/// Snapshots kept; taking one more drops the oldest.
pub const MAX_SNAPSHOTS: usize = 32;

/// Longest snapshot name, bytes.
pub const MAX_NAME_BYTES: usize = 64;

/// The `kind` tag of an exported snapshot.
pub const SNAPSHOT_KIND: &str = "regia.memory_snapshot";

/// Height of the snapshot list, points.
pub const SNAPSHOT_LIST_HEIGHT: f32 = 120.0;

/// One category (engine subsystem) of a snapshot: heap from the ledger, GPU from the allocator.
/// `None` when the source does not track it (no tagging allocator, no allocator report).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryRow {
    /// Subsystem name (`scene`, `render`, ...).
    pub name: String,
    /// Live heap bytes.
    pub heap_bytes: Option<u64>,
    /// Highest live heap bytes since start-up.
    pub heap_peak_bytes: Option<u64>,
    /// Live heap allocations.
    pub heap_allocations: Option<u64>,
    /// GPU bytes of the allocations labeled for it.
    pub gpu_bytes: Option<u64>,
}

/// A named count: the entities of an archetype or a component.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CountRow {
    /// For an archetype its component type names (sorted, joined by `, `); for a component its
    /// type name.
    pub name: String,
    /// Entities.
    pub entities: u64,
}

/// The assets of one kind.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetRow {
    /// Asset type name.
    pub kind: String,
    /// Loaded assets.
    pub loaded: u64,
    /// Their bytes.
    pub bytes: u64,
}

/// One named memory snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedSnapshot {
    /// [`SNAPSHOT_KIND`].
    pub kind: String,
    /// The name it was taken under (unique in the list).
    pub name: String,
    /// The how-many-th snapshot of this editor session (1 = first).
    pub index: u64,
    /// The Profiler source it was taken from (`edit`, `pie`, `process`, `remote`).
    pub source: String,
    /// Whether heap bytes are tracked (the tagging allocator is installed).
    pub heap_tracked: bool,
    /// Live heap bytes, all subsystems.
    pub heap_live_bytes: Option<u64>,
    /// Highest live heap bytes.
    pub heap_peak_bytes: Option<u64>,
    /// GPU allocator bytes.
    pub gpu_allocated_bytes: Option<u64>,
    /// Spawned entities.
    pub entities: u64,
    /// Per category (subsystem), in report order.
    pub categories: Vec<CategoryRow>,
    /// Entities per archetype (non-empty archetypes); `None` for a process source.
    pub archetypes: Option<Vec<CountRow>>,
    /// Entities per component type; `None` for a process source.
    pub components: Option<Vec<CountRow>>,
    /// Loaded assets per kind; `None` without an asset server.
    pub assets: Option<Vec<AssetRow>>,
}

/// One category's change between two snapshots.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CategoryDelta {
    /// Subsystem name.
    pub name: String,
    /// Live heap bytes gained (negative: freed).
    pub heap_bytes: i64,
    /// Live heap allocations gained.
    pub heap_allocations: i64,
    /// GPU bytes gained.
    pub gpu_bytes: i64,
}

impl CategoryDelta {
    /// Heap plus GPU bytes gained: the sort key.
    #[must_use]
    pub fn growth(&self) -> i64 {
        self.heap_bytes.saturating_add(self.gpu_bytes)
    }
}

/// One count's change between two snapshots.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CountDelta {
    /// The archetype, component or asset kind ([`CountRow::name`], [`AssetRow::kind`]).
    pub name: String,
    /// The count in the earlier snapshot.
    pub a: u64,
    /// The count in the later snapshot.
    pub b: u64,
    /// `b - a`.
    pub delta: i64,
    /// Asset bytes gained (asset kinds only, else 0).
    pub bytes: i64,
}

/// The difference from snapshot `a` to snapshot `b` ([`diff`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SnapshotDiff {
    /// The earlier snapshot's name.
    pub a: String,
    /// The later snapshot's name.
    pub b: String,
    /// Live heap bytes gained (`None` unless both track the heap).
    pub heap_bytes: Option<i64>,
    /// GPU allocator bytes gained (`None` unless both have a report).
    pub gpu_bytes: Option<i64>,
    /// Entities gained.
    pub entities: i64,
    /// The categories that changed, most growth first (ties by name).
    pub categories: Vec<CategoryDelta>,
    /// The archetypes whose entity count changed, most growth first (`None` unless both have
    /// archetype rows).
    pub archetypes: Option<Vec<CountDelta>>,
    /// The component types whose entity count changed, most growth first.
    pub components: Option<Vec<CountDelta>>,
    /// The asset kinds whose loaded count or bytes changed, most growth first.
    pub assets: Option<Vec<CountDelta>>,
}

/// Which half of the Memory tab is shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MemoryView {
    /// Categories with current and peak bytes, snapshots and their diff.
    #[default]
    Snapshots,
    /// The GPU resource browser ([`crate::gpu_browser`]).
    GpuResources,
}

/// The Memory tab's state (a resource of the editor world, created on first use).
#[derive(Resource, Clone, Debug, Default)]
pub struct MemoryBrowser {
    /// The snapshots, oldest first (at most [`MAX_SNAPSHOTS`]).
    pub snapshots: Vec<NamedSnapshot>,
    /// Snapshots taken this session (the next one's index is this plus one).
    pub taken: u64,
    /// The diff's earlier side (a snapshot name).
    pub diff_a: Option<String>,
    /// The diff's later side (a snapshot name).
    pub diff_b: Option<String>,
    /// The name field of **Take snapshot** (empty: `Snapshot <n>`).
    pub name: String,
    /// The last status line (snapshot taken, file written, an error).
    pub status: String,
    /// Highest GPU bytes seen per category while the editor ran.
    pub gpu_peaks: BTreeMap<String, u64>,
    /// The focusable snapshot list.
    pub keys: crate::canvas_keys::ListKeys,
    /// Which half of the tab is shown.
    pub view: MemoryView,
}

/// Why a snapshot call failed; [`Self::code`] is the agent protocol's reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotError {
    /// A name that is empty after trimming, too long, or has control characters.
    BadName(String),
    /// A snapshot of that name exists.
    NameTaken(String),
    /// No snapshot of that name.
    Unknown(String),
    /// The source has no memory report yet (a process source before its first answer).
    NoReport(String),
    /// The export path is absolute, leaves the project, or does not end in `.json`.
    BadPath(String),
    /// No project is open to export under.
    NoProject,
    /// The file could not be written.
    Write(String),
}

impl SnapshotError {
    /// The stable lowercase reason (`book/agents/profiler.md`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadName(_) => "memory_bad_name",
            Self::NameTaken(_) => "memory_name_taken",
            Self::Unknown(_) => "memory_unknown_snapshot",
            Self::NoReport(_) => "memory_no_report",
            Self::BadPath(_) => "memory_bad_path",
            Self::NoProject => "memory_no_project",
            Self::Write(_) => "memory_write_failed",
        }
    }

    /// The human-readable reason.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::BadName(m)
            | Self::NameTaken(m)
            | Self::Unknown(m)
            | Self::NoReport(m)
            | Self::BadPath(m) => m.clone(),
            Self::NoProject => "no project is open".to_owned(),
            Self::Write(m) => m.clone(),
        }
    }
}

/// The category rows of a `memory.report` answer.
fn categories_of(report: &Value) -> Vec<CategoryRow> {
    report["subsystems"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| CategoryRow {
            name: row["name"].as_str().unwrap_or("?").to_owned(),
            heap_bytes: row["heap_bytes"].as_u64(),
            heap_peak_bytes: row["heap_peak_bytes"].as_u64(),
            heap_allocations: row["heap_allocations"].as_u64(),
            gpu_bytes: row["gpu_bytes"].as_u64(),
        })
        .collect()
}

/// The asset rows of a `memory.report` answer (`None` without an asset server).
fn assets_of(report: &Value) -> Option<Vec<AssetRow>> {
    report["assets_by_type"].as_array().map(|rows| {
        rows.iter()
            .map(|row| AssetRow {
                kind: row["type"].as_str().unwrap_or("?").to_owned(),
                loaded: row["loaded"].as_u64().unwrap_or(0),
                bytes: row["bytes"].as_u64().unwrap_or(0),
            })
            .collect()
    })
}

/// Entities per non-empty archetype (named by its sorted component type names) and per component
/// type, both sorted by name.
#[must_use]
pub fn object_counts(world: &World) -> (Vec<CountRow>, Vec<CountRow>) {
    let name_of = |id: ComponentId| {
        world
            .components()
            .get_info(id)
            .map_or_else(|| format!("{id:?}"), |info| info.name().to_string())
    };
    let mut archetypes: BTreeMap<String, u64> = BTreeMap::new();
    let mut components: BTreeMap<String, u64> = BTreeMap::new();
    for archetype in world.archetypes().iter() {
        let n = u64::from(archetype.len());
        if n == 0 {
            continue;
        }
        let mut names: Vec<String> = archetype.components().iter().map(|&id| name_of(id)).collect();
        names.sort();
        for name in &names {
            *components.entry(name.clone()).or_default() += n;
        }
        *archetypes.entry(names.join(", ")).or_default() += n;
    }
    let rows = |map: BTreeMap<String, u64>| {
        map.into_iter()
            .map(|(name, entities)| CountRow { name, entities })
            .collect()
    };
    (rows(archetypes), rows(components))
}

/// A snapshot named `name` from a `memory.report` answer, plus the archetype and component rows of
/// `world` when given (an in-process source).
#[must_use]
pub fn snapshot_from_report(
    name: &str,
    index: u64,
    source: &str,
    report: &Value,
    world: Option<&World>,
) -> NamedSnapshot {
    let (archetypes, components) = match world {
        Some(world) => {
            let (a, c) = object_counts(world);
            (Some(a), Some(c))
        }
        None => (None, None),
    };
    let entities = match world {
        Some(world) => u64::from(world.entities().count_spawned()),
        None => report["ecs"]["entities"].as_u64().unwrap_or(0),
    };
    NamedSnapshot {
        kind: SNAPSHOT_KIND.to_owned(),
        name: name.to_owned(),
        index,
        source: source.to_owned(),
        heap_tracked: report["heap"]["tracked"].as_bool().unwrap_or(false),
        heap_live_bytes: report["heap"]["live_bytes"].as_u64(),
        heap_peak_bytes: report["heap"]["peak_bytes"].as_u64(),
        gpu_allocated_bytes: report["gpu"]["allocated_bytes"]
            .as_u64()
            .filter(|_| report["gpu"]["available"].as_bool().unwrap_or(false)),
        entities,
        categories: categories_of(report),
        archetypes,
        components,
        assets: assets_of(report),
    }
}

fn signed(now: u64, then: u64) -> i64 {
    (i128::from(now) - i128::from(then)).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// The rows of `a` and `b` matched by name, with the ones whose count changed, most growth first.
fn count_deltas(a: &[CountRow], b: &[CountRow]) -> Vec<CountDelta> {
    let mut by: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    for row in a {
        by.entry(&row.name).or_default().0 = row.entities;
    }
    for row in b {
        by.entry(&row.name).or_default().1 = row.entities;
    }
    let mut out: Vec<CountDelta> = by
        .into_iter()
        .filter(|(_, (a, b))| a != b)
        .map(|(name, (a, b))| CountDelta {
            name: name.to_owned(),
            a,
            b,
            delta: signed(b, a),
            bytes: 0,
        })
        .collect();
    out.sort_by(|x, y| y.delta.cmp(&x.delta).then_with(|| x.name.cmp(&y.name)));
    out
}

fn asset_deltas(a: &[AssetRow], b: &[AssetRow]) -> Vec<CountDelta> {
    let mut by: BTreeMap<&str, (AssetRow, AssetRow)> = BTreeMap::new();
    for row in a {
        by.entry(&row.kind).or_default().0 = row.clone();
    }
    for row in b {
        by.entry(&row.kind).or_default().1 = row.clone();
    }
    let mut out: Vec<CountDelta> = by
        .into_iter()
        .filter(|(_, (a, b))| a.loaded != b.loaded || a.bytes != b.bytes)
        .map(|(kind, (a, b))| CountDelta {
            name: kind.to_owned(),
            a: a.loaded,
            b: b.loaded,
            delta: signed(b.loaded, a.loaded),
            bytes: signed(b.bytes, a.bytes),
        })
        .collect();
    out.sort_by(|x, y| {
        y.delta
            .cmp(&x.delta)
            .then_with(|| y.bytes.cmp(&x.bytes))
            .then_with(|| x.name.cmp(&y.name))
    });
    out
}

/// The change from snapshot `a` to snapshot `b`: totals, the categories that changed (most heap
/// plus GPU growth first), and the archetype, component and asset counts that changed (most growth
/// first). Deterministic: ties sort by name.
#[must_use]
pub fn diff(a: &NamedSnapshot, b: &NamedSnapshot) -> SnapshotDiff {
    let mut categories: Vec<CategoryDelta> = b
        .categories
        .iter()
        .map(|now| {
            let then = a
                .categories
                .iter()
                .find(|c| c.name == now.name)
                .cloned()
                .unwrap_or_default();
            let d = |x: Option<u64>, y: Option<u64>| signed(x.unwrap_or(0), y.unwrap_or(0));
            CategoryDelta {
                name: now.name.clone(),
                heap_bytes: d(now.heap_bytes, then.heap_bytes),
                heap_allocations: d(now.heap_allocations, then.heap_allocations),
                gpu_bytes: d(now.gpu_bytes, then.gpu_bytes),
            }
        })
        .filter(|c| c.heap_bytes != 0 || c.heap_allocations != 0 || c.gpu_bytes != 0)
        .collect();
    categories.sort_by(|x, y| y.growth().cmp(&x.growth()).then_with(|| x.name.cmp(&y.name)));
    let both = |x: &Option<Vec<CountRow>>, y: &Option<Vec<CountRow>>| match (x, y) {
        (Some(x), Some(y)) => Some(count_deltas(x, y)),
        _ => None,
    };
    SnapshotDiff {
        a: a.name.clone(),
        b: b.name.clone(),
        heap_bytes: a
            .heap_live_bytes
            .zip(b.heap_live_bytes)
            .filter(|_| a.heap_tracked && b.heap_tracked)
            .map(|(then, now)| signed(now, then)),
        gpu_bytes: a
            .gpu_allocated_bytes
            .zip(b.gpu_allocated_bytes)
            .map(|(then, now)| signed(now, then)),
        entities: signed(b.entities, a.entities),
        categories,
        archetypes: both(&a.archetypes, &b.archetypes),
        components: both(&a.components, &b.components),
        assets: match (&a.assets, &b.assets) {
            (Some(x), Some(y)) => Some(asset_deltas(x, y)),
            _ => None,
        },
    }
}

/// `name` trimmed, or why it cannot name a snapshot.
fn checked_name(name: &str) -> Result<String, SnapshotError> {
    let name = name.trim();
    if name.is_empty() || name.len() > MAX_NAME_BYTES || name.chars().any(char::is_control) {
        return Err(SnapshotError::BadName(format!(
            "a snapshot name is 1 to {MAX_NAME_BYTES} bytes without control characters"
        )));
    }
    Ok(name.to_owned())
}

/// The memory report and the world to count objects in for the Profiler's current source: this
/// editor's for the in-process sources, the last report read from a process source.
fn source_report(world: &mut World) -> Result<(String, Value, bool), SnapshotError> {
    let source = world
        .get_resource::<ProfilerView>()
        .map(|v| v.source().clone())
        .unwrap_or_default();
    match source {
        ProfilerSource::Edit | ProfilerSource::Pie => {
            let report = regia_agent::memory_report(world, &json!({ "top": 0 }))
                .map_err(|e| SnapshotError::NoReport(e.message))?;
            Ok((source.kind().to_owned(), report, true))
        }
        ProfilerSource::Process(_) | ProfilerSource::Remote { .. } => {
            let report = world
                .get_resource::<ProfilerView>()
                .and_then(|v| v.frame_data.as_ref().and_then(|(_, d)| d.memory.clone()))
                .ok_or_else(|| {
                    SnapshotError::NoReport(
                        "the process source has no memory report yet: show the Memory tab until it reads one"
                            .to_owned(),
                    )
                })?;
            Ok((source.kind().to_owned(), report, false))
        }
    }
}

/// Takes a snapshot of the Profiler's current source under `name` (`None` or empty:
/// `Snapshot <n>`), adds it to the list (dropping the oldest past [`MAX_SNAPSHOTS`]) and returns
/// it. The [`MemoryBrowser`] resource is created before the counts are read, so taking the first
/// snapshot does not show up in a later diff.
///
/// # Errors
///
/// A bad or taken name, or a process source with no report yet.
pub fn take(world: &mut World, name: Option<&str>) -> Result<NamedSnapshot, SnapshotError> {
    let _span = tracing::info_span!("memory_snapshot_take").entered();
    world.get_resource_or_insert_with(MemoryBrowser::default);
    let browser = world.resource::<MemoryBrowser>();
    let index = browser.taken + 1;
    let name = match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => checked_name(name)?,
        None => {
            let mut k = index;
            while browser
                .snapshots
                .iter()
                .any(|s| s.name == format!("Snapshot {k}"))
            {
                k += 1;
            }
            format!("Snapshot {k}")
        }
    };
    if browser.snapshots.iter().any(|s| s.name == name) {
        return Err(SnapshotError::NameTaken(format!(
            "a snapshot named `{name}` exists"
        )));
    }
    let (source, report, local) = source_report(world)?;
    let snapshot = snapshot_from_report(&name, index, &source, &report, local.then_some(&*world));
    let mut browser = world.resource_mut::<MemoryBrowser>();
    browser.taken = index;
    browser.snapshots.push(snapshot.clone());
    let browser = &mut *browser;
    while browser.snapshots.len() > MAX_SNAPSHOTS {
        let dropped = browser.snapshots.remove(0);
        for side in [&mut browser.diff_a, &mut browser.diff_b] {
            if side.as_deref() == Some(dropped.name.as_str()) {
                *side = None;
            }
        }
    }
    Ok(snapshot)
}

/// The snapshot named `name`.
///
/// # Errors
///
/// [`SnapshotError::Unknown`].
pub fn find<'a>(browser: &'a MemoryBrowser, name: &str) -> Result<&'a NamedSnapshot, SnapshotError> {
    browser
        .snapshots
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| SnapshotError::Unknown(format!("no snapshot named `{name}`")))
}

/// The diff sides the tab shows: the chosen `diff_a` and `diff_b`, else the two newest snapshots.
#[must_use]
pub fn diff_sides(browser: &MemoryBrowser) -> Option<(String, String)> {
    let newest = |back: usize| {
        browser
            .snapshots
            .len()
            .checked_sub(back)
            .and_then(|i| browser.snapshots.get(i))
            .map(|s| s.name.clone())
    };
    let a = browser.diff_a.clone().or_else(|| newest(2))?;
    let b = browser.diff_b.clone().or_else(|| newest(1))?;
    (a != b).then_some((a, b))
}

/// The JSON text of `snapshot` (pretty, `\n`-terminated).
#[must_use]
pub fn export_text(snapshot: &NamedSnapshot) -> String {
    let mut text = serde_json::to_string_pretty(snapshot).unwrap_or_default();
    text.push('\n');
    text
}

/// A file name for `name`: ASCII letters, digits, `-` and `_` kept, the rest `_`.
fn file_stem(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Where an export of `name` goes: `path` (project-relative, ending in `.json`, no `..`) or
/// `captures/memory/<name>.json`, under `root`.
///
/// # Errors
///
/// [`SnapshotError::BadPath`].
pub fn export_path(root: &Path, name: &str, path: Option<&str>) -> Result<PathBuf, SnapshotError> {
    let Some(path) = path else {
        return Ok(root
            .join("captures")
            .join("memory")
            .join(format!("{}.json", file_stem(name))));
    };
    let rel = Path::new(path);
    let lexical_ok = rel
        .components()
        .all(|c| matches!(c, PathComponent::Normal(_) | PathComponent::CurDir));
    if !lexical_ok || !path.ends_with(".json") {
        return Err(SnapshotError::BadPath(format!(
            "`{path}` must be project-relative, without `..`, and end in .json"
        )));
    }
    Ok(root.join(rel))
}

/// Writes snapshot `name` as JSON to [`export_path`] (atomically: a temporary file, then a rename)
/// and returns the path.
///
/// # Errors
///
/// No project, an unknown snapshot, a bad path, or a failed write.
pub fn export(world: &World, name: &str, path: Option<&str>) -> Result<PathBuf, SnapshotError> {
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
        .ok_or(SnapshotError::NoProject)?;
    let browser = world
        .get_resource::<MemoryBrowser>()
        .ok_or_else(|| SnapshotError::Unknown(format!("no snapshot named `{name}`")))?;
    let snapshot = find(browser, name)?;
    let file = export_path(&root, name, path)?;
    let write = |e: std::io::Error| SnapshotError::Write(format!("{}: {e}", file.display()));
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(write)?;
    }
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, export_text(snapshot)).map_err(write)?;
    std::fs::rename(&tmp, &file).map_err(write)?;
    Ok(file)
}

/// Raises the session GPU peaks with the GPU bytes of the categories of `report`.
pub fn note_gpu_peaks(peaks: &mut BTreeMap<String, u64>, report: &Value) {
    for row in categories_of(report) {
        if let Some(bytes) = row.gpu_bytes {
            let peak = peaks.entry(row.name).or_default();
            *peak = (*peak).max(bytes);
        }
    }
}

/// `bytes` as KiB / MiB / GiB text, `-` when unknown.
#[must_use]
pub fn bytes_text(bytes: Option<u64>) -> String {
    match bytes {
        None => "-".to_owned(),
        Some(b) if b >= 1 << 30 => format!("{:.2} GiB", b as f64 / f64::from(1u32 << 30)),
        Some(b) if b >= 1 << 20 => format!("{:.2} MiB", b as f64 / f64::from(1u32 << 20)),
        Some(b) if b >= 1 << 10 => format!("{:.1} KiB", b as f64 / 1024.0),
        Some(b) => format!("{b} B"),
    }
}

/// A signed byte change as text (`+1.00 MiB`, `-512 B`).
#[must_use]
pub fn delta_text(delta: i64) -> String {
    let sign = if delta < 0 { "-" } else { "+" };
    format!("{sign}{}", bytes_text(Some(delta.unsigned_abs())))
}

/// The accessible name of snapshot row `s`.
#[must_use]
pub fn snapshot_row_name(s: &NamedSnapshot, a: bool, b: bool) -> String {
    let side = match (a, b) {
        (true, _) => ", diff from",
        (_, true) => ", diff to",
        _ => "",
    };
    format!(
        "{}, {} entities, heap {}, GPU {}{side}",
        s.name,
        s.entities,
        bytes_text(s.heap_live_bytes),
        bytes_text(s.gpu_allocated_bytes)
    )
}

/// The short type name of each component of an archetype row (`a::B, c::D` → `B, D`).
fn short_names(name: &str) -> String {
    name.split(", ")
        .map(|n| n.rsplit("::").next().unwrap_or(n))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Draws the Memory tab: the view switch, then the snapshot half or the GPU resource browser.
pub fn memory_tab(ui: &mut egui::Ui, world: &mut World, report: Option<&Value>) {
    use crate::agent_parity::{PROFILER_MEMORY_VIEW, action_selectable};
    let view = world.get_resource_or_insert_with(MemoryBrowser::default).view;
    ui.horizontal(|ui| {
        for (v, text) in [
            (MemoryView::Snapshots, "Categories and snapshots"),
            (MemoryView::GpuResources, "GPU resources"),
        ] {
            // View-only switch; `editor.memory_snapshot` and `editor.gpu_resources` read each half.
            if action_selectable(ui, &PROFILER_MEMORY_VIEW, text, view == v).clicked() {
                world.resource_mut::<MemoryBrowser>().view = v;
            }
        }
    });
    ui.separator();
    match view {
        MemoryView::Snapshots => snapshots_ui(ui, world, report),
        MemoryView::GpuResources => crate::gpu_browser::browser_ui(ui, world),
    }
}

fn snapshots_ui(ui: &mut egui::Ui, world: &mut World, report: Option<&Value>) {
    use crate::agent_parity::{
        PROFILER_MEMORY_EXPORT, PROFILER_MEMORY_SET_A, PROFILER_MEMORY_SET_B, PROFILER_MEMORY_TAKE,
        action_button, action_enabled,
    };
    let tokens = crate::theme::ui_tokens(ui.ctx());
    let mut browser = world.resource::<MemoryBrowser>().clone();
    if let Some(report) = report {
        note_gpu_peaks(&mut browser.gpu_peaks, report);
    }
    egui::ScrollArea::vertical()
        .id_salt("profiler_memory")
        .show(ui, |ui| {
            match report {
                None => {
                    ui.label("No memory report yet.");
                }
                Some(m) => {
                    if m["heap"]["tracked"] == false {
                        ui.weak("Heap bytes need the tagging allocator (dev builds); GPU bytes and asset types still show.");
                    }
                    categories_ui(ui, m, &browser.gpu_peaks);
                }
            }
            ui.separator();
            ui.strong("Snapshots");
            ui.horizontal(|ui| {
                ui.label("Name");
                // Agent twin: `editor.memory_snapshot { action: "take", name }`.
                let field = ui.add(
                    egui::TextEdit::singleline(&mut browser.name)
                        .hint_text("Snapshot <n>")
                        .desired_width(160.0),
                );
                let _ = crate::a11y_audit::name(field, egui::WidgetType::TextEdit, "Snapshot name");
                if action_button(ui, &PROFILER_MEMORY_TAKE).clicked()
                    && crate::canvas_keys::first_in_this_frame(ui, egui::Id::new("memory-take"))
                {
                    let name = std::mem::take(&mut browser.name);
                    world.insert_resource(browser.clone());
                    browser.status = match take(world, Some(&name)) {
                        Ok(s) => format!("took {}", s.name),
                        Err(e) => {
                            browser.name = name;
                            e.message()
                        }
                    };
                    let latest = world.resource::<MemoryBrowser>().clone();
                    browser.snapshots = latest.snapshots;
                    browser.taken = latest.taken;
                    browser.diff_a = latest.diff_a;
                    browser.diff_b = latest.diff_b;
                }
            });
            let sides = diff_sides(&browser);
            let names: Vec<String> = browser
                .snapshots
                .iter()
                .map(|s| {
                    let (a, b) = sides.clone().unzip();
                    snapshot_row_name(s, a.as_deref() == Some(&s.name), b.as_deref() == Some(&s.name))
                })
                .collect();
            let name = |i: usize| names.get(i).cloned().unwrap_or_default();
            let current = browser.keys.current;
            let _ = crate::canvas_keys::list_rows(
                ui,
                "memory-snapshots",
                &mut browser.keys,
                names.len(),
                SNAPSHOT_LIST_HEIGHT,
                current,
                &name,
            );
            let row = browser.keys.current.and_then(|i| browser.snapshots.get(i)).map(|s| s.name.clone());
            ui.horizontal(|ui| {
                let some = row.is_some();
                if action_enabled(ui, some, &PROFILER_MEMORY_SET_A, None).clicked() {
                    browser.diff_a.clone_from(&row);
                }
                if action_enabled(ui, some, &PROFILER_MEMORY_SET_B, None).clicked() {
                    browser.diff_b.clone_from(&row);
                }
                if action_enabled(ui, some, &PROFILER_MEMORY_EXPORT, None).clicked()
                    && crate::canvas_keys::first_in_this_frame(ui, egui::Id::new("memory-export"))
                    && let Some(row) = &row
                {
                    world.insert_resource(browser.clone());
                    browser.status = match export(world, row, None) {
                        Ok(path) => format!("wrote {}", path.display()),
                        Err(e) => e.message(),
                    };
                }
            });
            if !browser.status.is_empty() {
                ui.weak(&browser.status);
            }
            let Some((a, b)) = diff_sides(&browser) else {
                ui.weak("Take two snapshots to see what grew between them.");
                return;
            };
            let (Ok(sa), Ok(sb)) = (find(&browser, &a), find(&browser, &b)) else {
                return;
            };
            diff_ui(ui, &diff(sa, sb), tokens.accents.alert, tokens.accents.ok);
        });
    world.insert_resource(browser);
}

fn categories_ui(ui: &mut egui::Ui, m: &Value, gpu_peaks: &BTreeMap<String, u64>) {
    egui::Grid::new("profiler_mem_subsystems")
        .striped(true)
        .show(ui, |ui| {
            for h in ["category", "heap", "heap peak", "GPU", "GPU peak"] {
                ui.strong(h);
            }
            ui.end_row();
            for row in categories_of(m) {
                ui.monospace(&row.name);
                ui.monospace(bytes_text(row.heap_bytes));
                ui.monospace(bytes_text(row.heap_peak_bytes));
                ui.monospace(bytes_text(row.gpu_bytes));
                ui.monospace(bytes_text(gpu_peaks.get(&row.name).copied()));
                ui.end_row();
            }
        });
    ui.strong("Asset types");
    egui::Grid::new("profiler_mem_assets")
        .striped(true)
        .show(ui, |ui| {
            for row in assets_of(m).into_iter().flatten() {
                ui.monospace(row.kind);
                ui.monospace(format!("{} loaded", row.loaded));
                ui.monospace(bytes_text(Some(row.bytes)));
                ui.end_row();
            }
        });
    ui.strong("Owners");
    for row in m["owners"].as_array().into_iter().flatten() {
        ui.monospace(format!(
            "{:>12}  {}",
            bytes_text(row["heap_bytes"].as_u64()),
            row["name"].as_str().unwrap_or("?")
        ));
    }
}

fn diff_ui(ui: &mut egui::Ui, d: &SnapshotDiff, grew: egui::Color32, shrank: egui::Color32) {
    let tint = |v: i64| if v > 0 { grew } else { shrank };
    ui.strong(format!("{} → {}", d.a, d.b));
    ui.label(format!(
        "heap {} · GPU {} · entities {:+}",
        d.heap_bytes.map_or_else(|| "-".to_owned(), delta_text),
        d.gpu_bytes.map_or_else(|| "-".to_owned(), delta_text),
        d.entities
    ));
    egui::Grid::new("memory_diff_categories")
        .striped(true)
        .show(ui, |ui| {
            for h in ["category", "heap", "allocations", "GPU"] {
                ui.strong(h);
            }
            ui.end_row();
            for c in &d.categories {
                ui.monospace(&c.name);
                ui.colored_label(tint(c.heap_bytes), delta_text(c.heap_bytes));
                ui.monospace(format!("{:+}", c.heap_allocations));
                ui.colored_label(tint(c.gpu_bytes), delta_text(c.gpu_bytes));
                ui.end_row();
            }
        });
    let counts = |ui: &mut egui::Ui, title: &str, id: &str, rows: &Option<Vec<CountDelta>>, short: bool| {
        ui.strong(title);
        let Some(rows) = rows else {
            ui.weak("Not available for a process source.");
            return;
        };
        if rows.is_empty() {
            ui.weak("No change.");
            return;
        }
        egui::Grid::new(id).striped(true).show(ui, |ui| {
            for r in rows {
                let shown = if short {
                    short_names(&r.name)
                } else {
                    r.name.clone()
                };
                let _ = ui.monospace(shown).on_hover_text(&r.name);
                ui.monospace(format!("{} → {}", r.a, r.b));
                ui.colored_label(tint(r.delta), format!("{:+}", r.delta));
                if r.bytes != 0 {
                    ui.monospace(delta_text(r.bytes));
                }
                ui.end_row();
            }
        });
    };
    counts(
        ui,
        "Archetypes (entities)",
        "memory_diff_archetypes",
        &d.archetypes,
        true,
    );
    counts(
        ui,
        "Components (entities)",
        "memory_diff_components",
        &d.components,
        true,
    );
    counts(ui, "Asset kinds (loaded)", "memory_diff_assets", &d.assets, false);
}

fn refused(e: &SnapshotError) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        e.code(),
        e.message(),
        "see book/agents/profiler.md",
    )
}

/// `editor.memory_snapshot { action, name?, a?, b?, path? }`.
fn memory_snapshot(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = params.get("action").and_then(Value::as_str).unwrap_or_default();
    let text = |key: &str| params.get(key).and_then(Value::as_str);
    match action {
        "take" => {
            let s = take(world, text("name")).map_err(|e| refused(&e))?;
            Ok(json!({ "snapshot": s }))
        }
        "list" => {
            let browser = world.get_resource::<MemoryBrowser>().cloned().unwrap_or_default();
            let rows: Vec<Value> = browser
                .snapshots
                .iter()
                .map(|s| {
                    json!({
                        "name": s.name, "index": s.index, "source": s.source,
                        "entities": s.entities, "heap_live_bytes": s.heap_live_bytes,
                        "gpu_allocated_bytes": s.gpu_allocated_bytes,
                    })
                })
                .collect();
            let sides = diff_sides(&browser);
            Ok(json!({
                "snapshots": rows,
                "diff": sides.map(|(a, b)| json!({ "a": a, "b": b })),
            }))
        }
        "diff" => {
            let browser = world.get_resource::<MemoryBrowser>().cloned().unwrap_or_default();
            let (a, b) = match (text("a"), text("b")) {
                (Some(a), Some(b)) => (a.to_owned(), b.to_owned()),
                (None, None) => diff_sides(&browser).ok_or_else(|| {
                    refused(&SnapshotError::Unknown(
                        "no two snapshots to diff: take two, or pass a and b".to_owned(),
                    ))
                })?,
                _ => {
                    return Err(refused(&SnapshotError::Unknown(
                        "pass both a and b, or neither".to_owned(),
                    )));
                }
            };
            let sa = find(&browser, &a).map_err(|e| refused(&e))?;
            let sb = find(&browser, &b).map_err(|e| refused(&e))?;
            let d = diff(sa, sb);
            if let Some(mut live) = world.get_resource_mut::<MemoryBrowser>() {
                live.diff_a = Some(a);
                live.diff_b = Some(b);
            }
            Ok(json!({ "diff": d }))
        }
        "export" => {
            let name = text("name")
                .ok_or_else(|| refused(&SnapshotError::Unknown("export needs `name`".to_owned())))?;
            let path = export(world, name, text("path")).map_err(|e| refused(&e))?;
            Ok(json!({ "path": path.display().to_string() }))
        }
        _ => Err(RpcError::new(
            INVALID_PARAMS,
            "memory_bad_action",
            "`action` must be take, list, diff or export",
            "see book/agents/profiler.md",
        )),
    }
}

/// The Memory tab's agent methods (ADR-8142).
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.memory_snapshot",
            tool: "memory_snapshot",
            description: "Memory snapshots of the editor Profiler's Memory tab: take records, under a name, the heap and GPU bytes per category (subsystem) with the heap peak, the entities per archetype and per component, and the loaded assets per kind of the Profiler's source (archetypes and components only for the in-process sources); list names them; diff compares two (per-category growth, most first, and the archetype, component and asset counts that changed: the object-leak view); export writes one as JSON under the project.",
            params: &[
                ParamSchema {
                    name: "action",
                    kind: "string",
                    required: true,
                    description: "take, list, diff or export.",
                },
                ParamSchema {
                    name: "name",
                    kind: "string",
                    required: false,
                    description: "take: the new snapshot's name (1-64 bytes; default Snapshot <n>); export: the snapshot to write.",
                },
                ParamSchema {
                    name: "a",
                    kind: "string",
                    required: false,
                    description: "diff: the earlier snapshot (with b; both absent: the tab's sides, else the two newest).",
                },
                ParamSchema {
                    name: "b",
                    kind: "string",
                    required: false,
                    description: "diff: the later snapshot.",
                },
                ParamSchema {
                    name: "path",
                    kind: "string",
                    required: false,
                    description: "export: project-relative .json path (default captures/memory/<name>.json).",
                },
            ],
            result: "take: { snapshot }; list: { snapshots: [{ name, index, source, entities, heap_live_bytes, gpu_allocated_bytes }], diff: { a, b } | null }; diff: { diff: { a, b, heap_bytes, gpu_bytes, entities, categories: [{ name, heap_bytes, heap_allocations, gpu_bytes }], archetypes, components, assets: [{ name, a, b, delta, bytes }] | null } }; export: { path }",
            mutating: true,
            permission: Permission::EditScene,
        },
        memory_snapshot as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Component)]
    struct LeakA;
    #[derive(Component)]
    struct LeakB;

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(ProfilerView::default());
        world
    }

    /// Acceptance (P-308): N entities spawned between two snapshots show as exactly +N in their
    /// archetype and in each of its components, and nowhere else.
    #[test]
    fn n_entities_between_two_snapshots_show_as_plus_n_in_their_archetype() {
        let mut world = world();
        for _ in 0..3 {
            world.spawn(LeakA);
        }
        let first = take(&mut world, Some("before")).expect("first snapshot");
        const N: usize = 17;
        for _ in 0..N {
            world.spawn((LeakA, LeakB));
        }
        let second = take(&mut world, Some("after")).expect("second snapshot");
        assert_eq!((first.index, second.index), (1, 2));
        let d = diff(&first, &second);
        assert_eq!(d.entities, N as i64, "{d:?}");
        let archetypes = d.archetypes.expect("in-process rows");
        assert_eq!(
            archetypes.len(),
            1,
            "only the new archetype changed: {archetypes:?}"
        );
        let row = &archetypes[0];
        assert!(
            row.name.contains("LeakA") && row.name.contains("LeakB"),
            "{row:?}"
        );
        assert_eq!((row.a, row.b, row.delta), (0, N as u64, N as i64));
        let components = d.components.expect("in-process rows");
        let delta_of = |short: &str| {
            components
                .iter()
                .find(|c| c.name.ends_with(short))
                .map(|c| c.delta)
        };
        assert_eq!(delta_of("LeakA"), Some(N as i64));
        assert_eq!(delta_of("LeakB"), Some(N as i64));
        assert_eq!(components.len(), 2, "{components:?}");
        // The diff through the agent method agrees.
        let out = memory_snapshot(
            &mut world,
            &json!({ "action": "diff", "a": "before", "b": "after" }),
        )
        .expect("diff");
        assert_eq!(out["diff"]["archetypes"][0]["delta"], json!(N));
        assert_eq!(out["diff"]["entities"], json!(N));
    }

    #[test]
    fn categories_sort_by_growth_and_carry_peaks() {
        let report = |scene: u64, render_gpu: u64| {
            json!({
                "heap": { "tracked": true, "live_bytes": 1000 + scene, "peak_bytes": 9000 },
                "gpu": { "available": true, "allocated_bytes": render_gpu },
                "ecs": { "entities": 4 },
                "subsystems": [
                    { "name": "scene", "heap_bytes": scene, "heap_peak_bytes": 5000, "heap_allocations": 1, "gpu_bytes": 0 },
                    { "name": "render", "heap_bytes": 10, "heap_peak_bytes": 10, "heap_allocations": 1, "gpu_bytes": render_gpu },
                    { "name": "audio", "heap_bytes": 7, "heap_peak_bytes": 7, "heap_allocations": 1, "gpu_bytes": 0 },
                ],
                "assets_by_type": [ { "type": "Mesh", "loaded": 2, "bytes": 100 } ],
            })
        };
        let a = snapshot_from_report("a", 1, "process", &report(100, 1 << 20), None);
        let b = snapshot_from_report("b", 2, "process", &report(4100, 3 << 20), None);
        assert_eq!(a.categories[0].heap_peak_bytes, Some(5000));
        assert!(a.archetypes.is_none() && a.components.is_none());
        let d = diff(&a, &b);
        let names: Vec<&str> = d.categories.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            ["render", "scene"],
            "audio did not change; render grew most"
        );
        assert_eq!(d.categories[0].gpu_bytes, 2 << 20);
        assert_eq!(d.heap_bytes, Some(4000));
        assert_eq!(d.gpu_bytes, Some(2 << 20));
        assert_eq!(d.archetypes, None, "a process source has no archetype rows");
        assert_eq!(d.assets, Some(Vec::new()), "assets did not change");
        let mut peaks = BTreeMap::new();
        note_gpu_peaks(&mut peaks, &report(0, 3 << 20));
        note_gpu_peaks(&mut peaks, &report(0, 1 << 20));
        assert_eq!(
            peaks.get("render"),
            Some(&(3 << 20)),
            "the peak stays at the highest seen"
        );
    }

    #[test]
    fn names_are_checked_unique_and_defaulted() {
        let mut world = world();
        let s = take(&mut world, None).expect("default name");
        assert_eq!(s.name, "Snapshot 1");
        assert_eq!(
            take(&mut world, Some("Snapshot 1"))
                .map(|s| s.name)
                .map_err(|e| e.code()),
            Err("memory_name_taken")
        );
        assert_eq!(
            take(&mut world, Some("bad\nname")).map_err(|e| e.code()).err(),
            Some("memory_bad_name")
        );
        let long = "x".repeat(MAX_NAME_BYTES + 1);
        assert_eq!(
            take(&mut world, Some(&long)).map_err(|e| e.code()).err(),
            Some("memory_bad_name")
        );
        // A refused take does not advance the count.
        assert_eq!(take(&mut world, None).expect("second").name, "Snapshot 2");
    }

    #[test]
    fn the_list_keeps_at_most_max_snapshots_and_forgets_dropped_sides() {
        let mut world = world();
        take(&mut world, Some("first")).expect("first");
        world.resource_mut::<MemoryBrowser>().diff_a = Some("first".into());
        for k in 0..MAX_SNAPSHOTS {
            take(&mut world, Some(&format!("s{k}"))).expect("take");
        }
        let browser = world.resource::<MemoryBrowser>();
        assert_eq!(browser.snapshots.len(), MAX_SNAPSHOTS);
        assert_eq!(browser.snapshots[0].name, "s0");
        assert_eq!(browser.diff_a, None);
        assert_eq!(
            diff_sides(browser),
            Some((
                format!("s{}", MAX_SNAPSHOTS - 2),
                format!("s{}", MAX_SNAPSHOTS - 1)
            ))
        );
    }

    #[test]
    fn export_writes_json_under_the_project_and_refuses_escapes() {
        let dir = regia_test_utils::unique_temp_dir("memory-browser-export");
        let mut world = world();
        assert_eq!(
            export(&world, "x", None).map_err(|e| e.code()).err(),
            Some("memory_no_project")
        );
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
        take(&mut world, Some("level a/b")).expect("take");
        let path = export(&world, "level a/b", None).expect("export");
        assert_eq!(path, dir.join("captures/memory/level_a_b.json"));
        let text = std::fs::read_to_string(&path).expect("read back");
        let back: NamedSnapshot = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(back.kind, SNAPSHOT_KIND);
        assert_eq!(back.name, "level a/b");
        for bad in ["../out.json", "/tmp/out.json", "out.txt"] {
            assert_eq!(
                export(&world, "level a/b", Some(bad)).map_err(|e| e.code()).err(),
                Some("memory_bad_path"),
                "{bad}"
            );
        }
        assert_eq!(
            export(&world, "nope", None).map_err(|e| e.code()).err(),
            Some("memory_unknown_snapshot")
        );
        let out = memory_snapshot(
            &mut world,
            &json!({ "action": "export", "name": "level a/b", "path": "mem/a.json" }),
        )
        .expect("export through the method");
        assert!(
            out["path"].as_str().is_some_and(|p| p.ends_with("mem/a.json")),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_method_lists_and_refuses_bad_actions() {
        let mut world = world();
        let out = memory_snapshot(&mut world, &json!({ "action": "list" })).expect("list");
        assert_eq!(out["snapshots"], json!([]));
        assert_eq!(out["diff"], Value::Null);
        let err = memory_snapshot(&mut world, &json!({ "action": "nope" })).expect_err("bad action");
        assert!(format!("{err:?}").contains("memory_bad_action"), "{err:?}");
        let err = memory_snapshot(&mut world, &json!({ "action": "diff" })).expect_err("nothing to diff");
        assert!(format!("{err:?}").contains("memory_unknown_snapshot"), "{err:?}");
        take(&mut world, Some("one")).expect("take");
        take(&mut world, Some("two")).expect("take");
        let out = memory_snapshot(&mut world, &json!({ "action": "list" })).expect("list");
        assert_eq!(out["diff"], json!({ "a": "one", "b": "two" }));
        assert_eq!(out["snapshots"][1]["index"], json!(2));
        let err =
            memory_snapshot(&mut world, &json!({ "action": "diff", "a": "one" })).expect_err("half a pair");
        assert!(format!("{err:?}").contains("memory_unknown_snapshot"), "{err:?}");
    }

    #[test]
    fn a_process_source_without_a_report_refuses_a_snapshot() {
        let dir = regia_test_utils::TempDir::new("memory-browser-process");
        let mut world = world();
        world.insert_resource(crate::pie_process::test_hooks::dedicated_session(&dir));
        crate::profiler_source::set_source(
            &mut world,
            ProfilerSource::Process(crate::pie_process::ProcessRole::Server),
        )
        .expect("the fake session accepts a process source");
        world.resource_mut::<ProfilerView>().frame_data = None;
        assert_eq!(
            take(&mut world, None).map_err(|e| e.code()).err(),
            Some("memory_no_report")
        );
    }

    /// The snapshot list and its buttons are reached and operated from the keyboard: Tab to
    /// **Take snapshot**, Enter takes one, Tab to the list, Down moves between snapshots, and the
    /// diff of the two shows (every widget named).
    #[test]
    fn the_snapshot_list_is_operable_from_the_keyboard() {
        use crate::canvas_keys::test_input::{focused_name, frame, key, tab_to};
        let ctx = egui::Context::default();
        let mut world = world();
        world.spawn(LeakA);
        let size = [700.0, 700.0];
        let draw = |ui: &mut egui::Ui, world: &mut World| {
            egui::CentralPanel::default().show(ui, |ui| memory_tab(ui, world, None));
        };
        frame(&ctx, size, vec![], |ui| draw(ui, &mut world));
        tab_to(&ctx, size, "Take snapshot", false, 20, |ui| draw(ui, &mut world));
        for _ in 0..2 {
            frame(
                &ctx,
                size,
                vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
                |ui| draw(ui, &mut world),
            );
            frame(&ctx, size, vec![], |ui| draw(ui, &mut world));
            world.spawn((LeakA, LeakB));
        }
        let names: Vec<String> = world
            .resource::<MemoryBrowser>()
            .snapshots
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert_eq!(
            names,
            ["Snapshot 1", "Snapshot 2"],
            "Enter on the button took two"
        );
        let update = frame(&ctx, size, vec![], |ui| draw(ui, &mut world));
        assert_eq!(crate::a11y_audit::unnamed_widgets(&update), Vec::<String>::new());
        let first = snapshot_row_name(&world.resource::<MemoryBrowser>().snapshots[0], true, false);
        let second = snapshot_row_name(&world.resource::<MemoryBrowser>().snapshots[1], false, true);
        tab_to(&ctx, size, &first, false, 20, |ui| draw(ui, &mut world));
        frame(
            &ctx,
            size,
            vec![key(egui::Key::ArrowDown, egui::Modifiers::NONE)],
            |ui| draw(ui, &mut world),
        );
        let down = frame(&ctx, size, vec![], |ui| draw(ui, &mut world));
        assert_eq!(focused_name(&down).as_deref(), Some(second.as_str()));
        let labels = crate::a11y_audit::label_names(&down);
        assert!(
            labels.iter().any(|n| n == "Snapshot 1 → Snapshot 2"),
            "the diff shows: {labels:?}"
        );
    }
}
