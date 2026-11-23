// SPDX-License-Identifier: MPL-2.0
//! Closure settings (CT-26, ADR-8543): authoring `[cook.chunk_options] content` and per-chunk
//! `entry` roots from Project Settings and `editor.content_chunks_set_rule`, with a preview of what
//! the edit does to the chunk assignment before anything is saved.
//!
//! A [`ClosurePreview`] is computed from the *edited* manifest text with the same pipeline calls the
//! cook and `regia assets chunks` make (`raw_universe`, `resolve_roots` over [`chunk_root_decls`],
//! [`assign_with_roots`]), and compared with the saved manifest's assignment: which assets move,
//! which stop or start shipping, which roots match nothing, and each asset's reason chain back to
//! the declaration that ships it.
//!
//! Previews run on a worker thread ([`PreviewWorker`]) so a large registry never stalls the editor
//! frame. Each request records a fingerprint of its inputs (the manifest on disk, the cooked
//! registry, the raw files under `assets/` and the edited text) when it is made; a result whose
//! inputs changed meanwhile is rejected as stale, and a result of an older request is dropped as
//! superseded. Requests made while one computes are coalesced: only the newest waits.
//!
//! A save through [`save_checked`] refuses a closure configuration whose roots do not resolve (an
//! `entry` glob that matches nothing, no entry point at all, a root that does not parse), so a
//! misleading "valid" state never reaches `Regia.toml`. The save returns a [`ManifestEdit`] undo
//! entry holding the exact file text before and after.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::chunks::{
    Assignment, ChunkError, ChunkRules, ContentMode, Reason, assign_with_roots, chunk_root_decls,
};
use regia_asset_pipeline::closure_roots::{RootInputs, RootReport, glob_match, raw_universe, resolve_roots};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::chunks_panel::{ChunksPanelError, refuse, rpc};
use crate::project_settings::ProjectSettingsState;
use crate::viewport_agent::Handler;

/// Most rows (moves, unshipped assets, roots) a preview's JSON lists by default.
pub const PREVIEW_ROWS: usize = 100;
/// Largest `limit` a paged method accepts.
pub const MAX_PAGE: usize = 1000;

/// Bumped every time a [`ManifestEdit`] rewrites `Regia.toml` (apply or revert), so open panels
/// can reload a manifest an undo changed under them.
static MANIFEST_REVISION: AtomicU64 = AtomicU64::new(0);

/// The current manifest revision ([`ManifestEdit`] writes so far in this process).
#[must_use]
pub fn manifest_revision() -> u64 {
    MANIFEST_REVISION.load(Ordering::Acquire)
}

/// A 64-bit FNV-1a hasher (deterministic across runs and platforms, unlike `DefaultHasher`).
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn bytes(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
        // Length-delimit so ("ab","c") and ("a","bc") differ.
        for b in (bytes.len() as u64).to_le_bytes() {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    fn u64(&mut self, n: u64) {
        self.bytes(&n.to_le_bytes());
    }
}

/// File metadata that changes when the file does: size and modification time.
fn stat(hasher: &mut Fnv, path: &Path) {
    match std::fs::metadata(path) {
        Ok(meta) => {
            hasher.u64(meta.len());
            let nanos = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            hasher.bytes(&nanos.to_le_bytes());
        }
        Err(_) => hasher.u64(u64::MAX),
    }
}

/// Every file under `dir` (sorted, recursive) with its size and modification time.
fn walk(hasher: &mut Fnv, dir: &Path, rel: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<(String, PathBuf, bool)> = entries
        .filter_map(Result::ok)
        .map(|e| {
            let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
            (e.file_name().to_string_lossy().into_owned(), e.path(), is_dir)
        })
        .collect();
    names.sort();
    for (name, path, is_dir) in names {
        let rel = format!("{rel}/{name}");
        hasher.bytes(rel.as_bytes());
        if is_dir {
            walk(hasher, &path, &rel);
        } else {
            stat(hasher, &path);
        }
    }
}

/// The fingerprint of a preview's inputs: the manifest on disk (its bytes), the cooked registry
/// and every file under `assets/` (size and modification time), and the edited manifest text.
/// Any change to a source or config file the preview read changes it.
#[must_use]
pub fn inputs_fingerprint(project: &Path, platform: &str, edited: &str) -> u64 {
    let mut hasher = Fnv::new();
    hasher.bytes(edited.as_bytes());
    let manifest = crate::project_settings::manifest_path(project);
    match std::fs::read(&manifest) {
        Ok(bytes) => hasher.bytes(&bytes),
        Err(_) => hasher.u64(u64::MAX),
    }
    let platform = if platform.is_empty() {
        regia_asset_pipeline::host_platform().to_string()
    } else {
        platform.to_owned()
    };
    let cache = regia_asset::cooked_cache_dir(project, &platform);
    stat(&mut hasher, &regia_asset::registry::cache_registry_path(&cache));
    walk(&mut hasher, &project.join("assets"), "assets");
    hasher.0
}

/// One declared root of the edited rules and whether it resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootStatus {
    /// The declaring item (`[project] startup_level`, `[cook.chunks.dlc] entry[0]`).
    pub declared_by: String,
    /// The path or glob as written.
    pub pattern: String,
    /// Whether it matched at least one raw file or cooked asset.
    pub matched: bool,
}

/// One asset whose chunk the edit changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignmentMove {
    /// `assets/...` path.
    pub path: String,
    /// Its chunk under the saved rules (`None`: unshipped, or the saved rules do not assign).
    pub from: Option<String>,
    /// Its chunk under the edited rules (`None`: unshipped).
    pub to: Option<String>,
}

/// Why a preview cannot be saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewProblem {
    /// `content_chunks_entry_unmatched`, `content_chunks_no_entry`, `content_chunks_rules` or a
    /// `content_root_*` code.
    pub code: &'static str,
    /// What is wrong.
    pub message: String,
}

/// The chunk assignment an edited manifest would cook, next to the saved one's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClosurePreview {
    /// The cooked platform.
    pub platform: String,
    /// `[cook.chunk_options] content` of the edited rules.
    pub content: ContentMode,
    /// Whether the edited rules cook: every root resolves and the assignment succeeds.
    pub valid: bool,
    /// Why not, when `valid` is false.
    pub problem: Option<PreviewProblem>,
    /// Each shipped asset's chunk (`assets/...` path to chunk name), as the cook assigns it.
    pub chunk_of: BTreeMap<String, String>,
    /// Shipped assets reached from another asset: the referrer on the shortest path.
    pub follows: BTreeMap<String, String>,
    /// Each shipped asset's reason (`rule`, `follows`, `shared`, `pinned`, `default`).
    pub reasons: BTreeMap<String, &'static str>,
    /// Assets that ship in no chunk, sorted.
    pub unshipped: Vec<String>,
    /// Assets whose chunk differs from the saved rules', sorted by path.
    pub moves: Vec<AssignmentMove>,
    /// Every declared root of the edited rules, in declaration order.
    pub roots: Vec<RootStatus>,
    /// The resolver's report: every root and raw file with the reason it exists.
    pub report: RootReport,
    /// [`inputs_fingerprint`] of the inputs the preview was requested against.
    pub fingerprint: u64,
}

impl ClosurePreview {
    /// The reason chain of `asset` (a path, with or without `assets/`): the asset, then each
    /// referrer back to the asset a rule or root selected, then the root's own chain through the
    /// raw files that named it, ending at the declaration (`[cook.chunks.dlc] entry[0]`). Empty when
    /// the asset is neither shipped nor a root.
    #[must_use]
    pub fn reason_chain(&self, asset: &str) -> Vec<String> {
        let path = if asset.starts_with("assets/") {
            asset.to_owned()
        } else {
            format!("assets/{asset}")
        };
        let records: BTreeMap<&str, &regia_asset_pipeline::closure_roots::RootRecord> = self
            .report
            .records
            .iter()
            .map(|r| (r.target.as_str(), r))
            .collect();
        if !self.chunk_of.contains_key(&path) && !records.contains_key(path.as_str()) {
            return Vec::new();
        }
        let mut chain = vec![path.clone()];
        let mut seen = BTreeSet::from([path.clone()]);
        let mut at = path;
        while let Some(via) = self.follows.get(&at) {
            if !seen.insert(via.clone()) {
                break;
            }
            chain.push(via.clone());
            at = via.clone();
        }
        // The root walk: `via` names the raw file (under `assets/`) that referenced this one.
        while let Some(record) = records.get(at.as_str()) {
            match &record.why.via {
                Some(via) => {
                    let next = format!("assets/{via}");
                    if !seen.insert(next.clone()) {
                        break;
                    }
                    chain.push(next.clone());
                    at = next;
                }
                None => {
                    chain.push(record.why.declared_by.clone());
                    break;
                }
            }
        }
        chain
    }
}

fn path_of(registry: &regia_asset::registry::AssetRegistry, id: regia_asset::AssetId) -> String {
    registry
        .by_id(id)
        .map_or_else(|| id.to_string(), |e| e.path.clone())
}

/// The `[project] startup_level` of a manifest text.
fn startup_level_of(table: &toml::Table) -> Option<String> {
    table
        .get("project")?
        .get("startup_level")?
        .as_str()
        .map(str::to_owned)
}

/// One side of a preview: rules, root evidence and (when the rules cook) the assignment.
struct Side {
    rules: ChunkRules,
    roots: Vec<RootStatus>,
    report: RootReport,
    assignment: Result<Assignment, PreviewProblem>,
}

fn side(
    project: &Path,
    registry: &regia_asset::registry::AssetRegistry,
    raw: &BTreeMap<String, u64>,
    text: &str,
) -> Result<Side, ChunksPanelError> {
    let table: toml::Table = text
        .parse()
        .or_else(|e| refuse("content_chunks_rules", format!("Regia.toml: {e}")))?;
    let rules = ChunkRules::from_table(&table).or_else(|e| refuse("content_chunks_rules", e.to_string()))?;
    let decls = chunk_root_decls(startup_level_of(&table).as_deref(), &rules);
    let cooked: Vec<(regia_asset::AssetId, String)> =
        registry.entries().map(|e| (e.id, e.path.clone())).collect();
    let inputs = RootInputs {
        assets_dir: project.join("assets"),
        raw,
        cooked: &cooked,
        exclude: &|_| false,
    };
    let evidence = match resolve_roots(&inputs, &decls, true) {
        Ok(evidence) => evidence,
        Err(e) => {
            return Ok(Side {
                roots: decls
                    .iter()
                    .map(|d| RootStatus {
                        declared_by: d.declared_by.clone(),
                        pattern: d.pattern.clone(),
                        matched: false,
                    })
                    .collect(),
                rules,
                report: RootReport::default(),
                assignment: Err(PreviewProblem {
                    code: e.code(),
                    message: e.to_string(),
                }),
            });
        }
    };
    let roots = decls
        .iter()
        .map(|d| RootStatus {
            declared_by: d.declared_by.clone(),
            pattern: d.pattern.clone(),
            matched: evidence.matched.contains(&d.declared_by),
        })
        .collect();
    let assignment = assign_with_roots(registry, &rules, &evidence).map_err(|e| PreviewProblem {
        code: match e {
            ChunkError::EntryMissing { .. } => "content_chunks_entry_unmatched",
            ChunkError::NoEntry => "content_chunks_no_entry",
            _ => "content_chunks_rules",
        },
        message: e.to_string(),
    });
    let report = evidence.report(&|id| path_of(registry, id));
    Ok(Side {
        rules,
        roots,
        report,
        assignment,
    })
}

/// Computes the preview of `edited` (a whole manifest text) against `saved` (the manifest on disk,
/// `None` when there is none) for `platform` (empty = host). `fingerprint` is stored as is.
///
/// # Errors
/// `content_chunks_bad_platform`, `content_chunks_no_cook`, `content_registry_invalid`,
/// `content_chunks_rules` (the edited text does not parse or its rules are invalid), or a
/// `content_root_*` code when the raw files cannot be listed.
pub fn compute_preview(
    project: &Path,
    platform: &str,
    saved: Option<&str>,
    edited: &str,
    fingerprint: u64,
) -> Result<ClosurePreview, ChunksPanelError> {
    let _span = tracing::info_span!(target: "regia_editor", "closure_preview").entered();
    let (platform, _cache, registry) = crate::chunks_panel::read_registry(project, platform)?;
    let raw = raw_universe(project).or_else(|e| refuse(e.code(), e.to_string()))?;
    let edited_side = side(project, &registry, &raw, edited)?;
    // The saved side is only the baseline of `moves`: a saved manifest that no longer cooks moves
    // every asset from "unassigned".
    let saved_assignment = saved
        .and_then(|text| side(project, &registry, &raw, text).ok())
        .and_then(|s| s.assignment.ok());
    let mut out = ClosurePreview {
        platform,
        content: edited_side.rules.content,
        roots: edited_side.roots,
        report: edited_side.report,
        fingerprint,
        ..ClosurePreview::default()
    };
    match edited_side.assignment {
        Ok(assignment) => {
            out.valid = true;
            for (id, chunk) in &assignment.chunk_of {
                let path = path_of(&registry, *id);
                if let Some(reason) = assignment.reason.get(id) {
                    out.reasons.insert(path.clone(), reason.kind());
                    if let Reason::Follows { via } = reason {
                        out.follows.insert(path.clone(), path_of(&registry, *via));
                    }
                }
                out.chunk_of.insert(path, chunk.clone());
            }
            out.unshipped = assignment
                .unshipped
                .iter()
                .map(|id| path_of(&registry, *id))
                .collect();
            out.unshipped.sort();
            let before: BTreeMap<String, String> = saved_assignment
                .map(|a| {
                    a.chunk_of
                        .iter()
                        .map(|(id, c)| (path_of(&registry, *id), c.clone()))
                        .collect()
                })
                .unwrap_or_default();
            let paths: BTreeSet<&String> = before.keys().chain(out.chunk_of.keys()).collect();
            out.moves = paths
                .into_iter()
                .filter_map(|path| {
                    let from = before.get(path).cloned();
                    let to = out.chunk_of.get(path).cloned();
                    (from != to).then(|| AssignmentMove {
                        path: path.clone(),
                        from,
                        to,
                    })
                })
                .collect();
            // Assets that were unshipped and stay unshipped are not moves; assets that shipped and
            // now do not are (`to: None`), covered by the union above.
        }
        Err(problem) => out.problem = Some(problem),
    }
    Ok(out)
}

/// A preview request.
#[derive(Clone, Debug)]
pub struct PreviewRequest {
    /// The project.
    pub project: PathBuf,
    /// Cooked platform (empty = host).
    pub platform: String,
    /// The edited manifest text.
    pub edited: String,
}

/// What [`PreviewWorker::poll`] found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PreviewStatus {
    /// Nothing requested yet.
    #[default]
    Idle,
    /// A request is computing (or queued behind one).
    Pending,
    /// [`PreviewWorker::latest`] is the preview of the newest request, and its inputs have not
    /// changed since it was requested.
    Ready,
    /// The newest request's inputs changed while it computed; the result was discarded.
    Stale,
    /// The newest request failed ([`PreviewWorker::error`]).
    Failed,
}

impl PreviewStatus {
    /// `idle`, `pending`, `ready`, `stale` or `failed`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Stale => "stale",
            Self::Failed => "failed",
        }
    }
}

/// What a [`PreviewWorker`] did; every counter is a deterministic function of the calls made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PreviewCounters {
    /// Requests made.
    pub requested: u64,
    /// Computations started on a worker thread.
    pub computed: u64,
    /// Queued requests replaced by a newer one before they started.
    pub coalesced: u64,
    /// Results dropped because a newer request was made while they computed.
    pub superseded: u64,
    /// Results dropped because their inputs changed while they computed.
    pub stale: u64,
    /// Results accepted.
    pub accepted: u64,
}

type PreviewResult = Result<ClosurePreview, ChunksPanelError>;

#[derive(Debug)]
struct InFlight {
    generation: u64,
    request: PreviewRequest,
    fingerprint: u64,
    answer: Receiver<PreviewResult>,
}

/// Computes closure previews off the editor thread, one at a time, newest request wins.
#[derive(Debug, Default)]
pub struct PreviewWorker {
    generation: u64,
    in_flight: Option<InFlight>,
    queued: Option<(u64, PreviewRequest, u64)>,
    status: PreviewStatus,
    latest: Option<Arc<ClosurePreview>>,
    error: Option<ChunksPanelError>,
    /// What the worker did.
    pub counters: PreviewCounters,
}

impl PreviewWorker {
    /// Requests a preview. The inputs' fingerprint is taken now, so any change to the manifest,
    /// the cooked registry or a file under `assets/` after this call makes the result stale. A
    /// request made while another computes waits for it (replacing any request already waiting).
    pub fn request(&mut self, request: PreviewRequest) {
        self.counters.requested += 1;
        self.generation += 1;
        let fingerprint = inputs_fingerprint(&request.project, &request.platform, &request.edited);
        self.status = PreviewStatus::Pending;
        if self.in_flight.is_some() {
            if self.queued.is_some() {
                self.counters.coalesced += 1;
            }
            self.queued = Some((self.generation, request, fingerprint));
        } else {
            self.start(self.generation, request, fingerprint);
        }
    }

    fn start(&mut self, generation: u64, request: PreviewRequest, fingerprint: u64) {
        let (send, answer) = std::sync::mpsc::channel();
        let job = request.clone();
        let spawned = std::thread::Builder::new()
            .name("regia-closure-preview".into())
            .spawn(move || {
                let saved =
                    std::fs::read_to_string(crate::project_settings::manifest_path(&job.project)).ok();
                let result = compute_preview(
                    &job.project,
                    &job.platform,
                    saved.as_deref(),
                    &job.edited,
                    fingerprint,
                );
                // The receiver is gone when the worker was dropped; nothing to report then.
                let _ = send.send(result);
            });
        match spawned {
            Ok(_) => {
                self.counters.computed += 1;
                self.in_flight = Some(InFlight {
                    generation,
                    request,
                    fingerprint,
                    answer,
                });
            }
            Err(e) => {
                self.status = PreviewStatus::Failed;
                self.error = Some(ChunksPanelError {
                    code: "content_chunks_preview_failed",
                    message: format!("cannot start the preview thread: {e}"),
                });
            }
        }
    }

    /// Takes a finished result, if any, without blocking.
    pub fn poll(&mut self) -> PreviewStatus {
        let Some(flight) = &self.in_flight else {
            return self.status;
        };
        match flight.answer.try_recv() {
            Ok(result) => self.finish(result),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.finish(refuse(
                "content_chunks_preview_failed",
                "the preview thread stopped without an answer",
            )),
        }
        self.status
    }

    /// Blocks until the newest request is resolved (ready, stale or failed).
    pub fn wait(&mut self) -> PreviewStatus {
        while let Some(flight) = &self.in_flight {
            let result = flight.answer.recv().unwrap_or_else(|_| {
                refuse(
                    "content_chunks_preview_failed",
                    "the preview thread stopped without an answer",
                )
            });
            self.finish(result);
        }
        self.status
    }

    fn finish(&mut self, result: PreviewResult) {
        let Some(flight) = self.in_flight.take() else {
            return;
        };
        if flight.generation != self.generation {
            self.counters.superseded += 1;
            if let Some((generation, request, fingerprint)) = self.queued.take() {
                self.start(generation, request, fingerprint);
            }
            return;
        }
        match result {
            Ok(preview) => {
                let now = inputs_fingerprint(
                    &flight.request.project,
                    &flight.request.platform,
                    &flight.request.edited,
                );
                if now == flight.fingerprint {
                    self.counters.accepted += 1;
                    self.latest = Some(Arc::new(preview));
                    self.error = None;
                    self.status = PreviewStatus::Ready;
                } else {
                    self.counters.stale += 1;
                    self.latest = None;
                    self.error = Some(ChunksPanelError {
                        code: "content_chunks_preview_stale",
                        message: "Regia.toml, the cooked registry or a file under assets/ changed while the \
                                  preview computed; request it again"
                            .into(),
                    });
                    self.status = PreviewStatus::Stale;
                }
            }
            Err(e) => {
                self.latest = None;
                self.error = Some(e);
                self.status = PreviewStatus::Failed;
            }
        }
    }

    /// The last accepted preview (cleared by a stale or failed result).
    #[must_use]
    pub fn latest(&self) -> Option<&Arc<ClosurePreview>> {
        self.latest.as_ref()
    }

    /// Why the newest request did not give a preview.
    #[must_use]
    pub fn error(&self) -> Option<&ChunksPanelError> {
        self.error.as_ref()
    }

    /// The status of the newest request.
    #[must_use]
    pub fn status(&self) -> PreviewStatus {
        self.status
    }
}

/// The undo entry of one `Regia.toml` write from the content-chunk settings: the exact file text
/// before and after. Applying writes `after`; reverting writes `before` (or removes the file when
/// there was none).
#[derive(Debug, Clone)]
pub struct ManifestEdit {
    path: PathBuf,
    before: Option<String>,
    after: Option<String>,
    source: String,
}

impl ManifestEdit {
    /// The manifest the entry rewrites.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, text: Option<&str>) {
        let result = match text {
            Some(text) => crate::level_io::write_atomic(&self.path, text.as_bytes()),
            None => std::fs::remove_file(&self.path).map_err(|e| e.to_string()),
        };
        match result {
            Ok(()) => {
                MANIFEST_REVISION.fetch_add(1, Ordering::AcqRel);
            }
            Err(e) => tracing::warn!(target: "regia_editor", "{}: {e}", self.path.display()),
        }
    }
}

impl crate::command::EditorCommand for ManifestEdit {
    fn apply(&mut self, _world: &mut World) {
        self.write(self.after.as_deref());
    }
    fn revert(&mut self, _world: &mut World) {
        self.write(self.before.as_deref());
    }
    fn merge(&mut self, _next: &dyn crate::command::EditorCommand) -> bool {
        false
    }
    fn changes_level_state(&self) -> bool {
        false
    }
    fn touched(&self) -> Vec<crate::command::CommandTouch> {
        vec![crate::command::CommandTouch::asset(
            self.path
                .file_name()
                .map_or_else(|| "Regia.toml".into(), |n| n.to_string_lossy().into_owned()),
            Some("cook.chunks".into()),
        )]
    }
    fn description(&self) -> String {
        format!("Edit content chunk settings ({})", self.source)
    }
}

/// Pushes a manifest write's undo entry on the editor's undo stack, when there is one.
pub fn push_manifest_undo(world: &mut World, command: ManifestEdit) {
    if let Some(mut stack) = world.get_resource_mut::<crate::command::UndoStack>() {
        stack.seal();
        stack.push(Box::new(command));
        stack.seal();
    }
}

/// What [`save_checked`] did.
#[derive(Debug)]
pub struct CheckedSave {
    /// The preview the save was checked against (`None` when no closure check was needed or no
    /// cook exists).
    pub preview: Option<ClosurePreview>,
    /// Whether the entry roots were checked against a cooked registry. `false` when the project
    /// has not been cooked: the roots are saved unverified and the next cook checks them.
    pub verified: bool,
    /// The undo entry of the write.
    pub undo: ManifestEdit,
}

/// Whether a manifest's chunk settings need the closure check before a save: closure mode, or any
/// `entry` root.
fn needs_closure_check(text: &str) -> bool {
    ChunkRules::from_manifest_toml(text).is_ok_and(|rules| {
        rules.content == ContentMode::Closure || rules.rules.values().any(|r| !r.entry.is_empty())
    })
}

/// Saves `settings` after checking its chunk settings as the cook would (`source` names who saved,
/// for the undo history): with closure mode or any `entry` root, the preview of the edited rules
/// must be valid, so an `entry` glob that matches nothing never saves as a working configuration.
///
/// # Errors
/// `content_chunks_entry_unmatched`, `content_chunks_no_entry`, a `content_root_*` code,
/// `content_chunks_rules` (invalid rules or another save validation), `content_chunks_bad_platform`
/// or `content_registry_invalid`. Nothing is written on error.
pub fn save_checked(
    settings: &mut ProjectSettingsState,
    project: &Path,
    platform: &str,
    source: &str,
) -> Result<CheckedSave, ChunksPanelError> {
    let Some(edited) = settings.manifest_text() else {
        return refuse("content_chunks_rules", "no Regia.toml is loaded");
    };
    let path = crate::project_settings::manifest_path(project);
    let before = std::fs::read_to_string(&path).ok();
    let mut preview = None;
    let mut verified = false;
    if needs_closure_check(&edited) {
        let fingerprint = inputs_fingerprint(project, platform, &edited);
        match compute_preview(project, platform, before.as_deref(), &edited, fingerprint) {
            Ok(p) => {
                if let Some(problem) = &p.problem {
                    return refuse(problem.code, problem.message.clone());
                }
                verified = true;
                preview = Some(p);
            }
            Err(e) if e.code == "content_chunks_no_cook" => {}
            Err(e) => return Err(e),
        }
    }
    settings
        .save(project)
        .or_else(|e| refuse("content_chunks_rules", e))?;
    MANIFEST_REVISION.fetch_add(1, Ordering::AcqRel);
    let after = std::fs::read_to_string(&path).ok();
    Ok(CheckedSave {
        preview,
        verified,
        undo: ManifestEdit {
            path,
            before,
            after,
            source: source.to_owned(),
        },
    })
}

/// The JSON form of a preview (`editor.content_chunks_preview`, the `preview` of
/// `editor.content_chunks_set_rule`): counts and the first `limit` rows of each list.
#[must_use]
pub fn preview_json(preview: &ClosurePreview, limit: usize) -> Value {
    let mut chunks: BTreeMap<&str, usize> = BTreeMap::new();
    for chunk in preview.chunk_of.values() {
        *chunks.entry(chunk.as_str()).or_default() += 1;
    }
    json!({
        "platform": preview.platform,
        "content": preview.content.name(),
        "valid": preview.valid,
        "error_code": preview.problem.as_ref().map(|p| p.code),
        "error": preview.problem.as_ref().map(|p| p.message.clone()),
        "shipped": preview.chunk_of.len(),
        "chunks": chunks,
        "unshipped_total": preview.unshipped.len(),
        "unshipped": preview.unshipped.iter().take(limit).collect::<Vec<_>>(),
        "moves_total": preview.moves.len(),
        "moves": preview
            .moves
            .iter()
            .take(limit)
            .map(|m| json!({ "path": m.path, "from": m.from, "to": m.to }))
            .collect::<Vec<_>>(),
        "roots": preview
            .roots
            .iter()
            .map(|r| json!({ "declared_by": r.declared_by, "pattern": r.pattern, "matched": r.matched }))
            .collect::<Vec<_>>(),
        "unmatched": preview
            .roots
            .iter()
            .filter(|r| !r.matched)
            .map(|r| r.declared_by.clone())
            .collect::<Vec<_>>(),
        "root_coverage": preview.report.coverage,
        "fingerprint": format!("{:016x}", preview.fingerprint),
    })
}

/// Applies the rule fields of an agent call (`chunk`, `include`, `entry`, `tags`, `requires`,
/// `delivery`, `priority`, `remove`, `content`) to `settings`; returns the chunk and whether it was
/// removed.
///
/// # Errors
/// `invalid_params` for a field of the wrong type or neither `chunk` nor `content`;
/// `content_chunks_bad_chunk` for removing a rule that does not exist.
pub(crate) fn apply_rule_params(
    settings: &mut ProjectSettingsState,
    params: &Value,
) -> Result<(Option<String>, bool), RpcError> {
    let chunk = params
        .get("chunk")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let content = match params.get("content") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .and_then(ContentMode::parse)
                .ok_or_else(|| RpcError::invalid_params("`content` must be \"all\" or \"closure\""))?,
        ),
    };
    if chunk.is_none() && content.is_none() {
        return Err(RpcError::invalid_params("`chunk` (or `content`) is required"));
    }
    if let Some(mode) = content {
        settings.set_content_mode(mode);
    }
    let Some(name) = chunk else {
        return Ok((None, false));
    };
    let removed = params.get("remove").and_then(Value::as_bool).unwrap_or(false);
    if removed {
        if !settings.remove_chunk_rule(&name) {
            return Err(rpc(ChunksPanelError {
                code: "content_chunks_bad_chunk",
                message: format!("no rule for chunk {name:?}"),
            }));
        }
        return Ok((Some(name), true));
    }
    let mut rule = settings
        .chunk_rules()
        .into_iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| crate::project_settings::ChunkRuleEdit {
            name: name.clone(),
            ..Default::default()
        });
    let list = crate::chunks_panel::string_list;
    if let Some(items) = list(params, "include")? {
        rule.include = items;
    }
    if let Some(items) = list(params, "entry")? {
        rule.entry = items;
    }
    if let Some(items) = list(params, "tags")? {
        rule.tags = items;
    }
    if let Some(items) = list(params, "requires")? {
        rule.requires = items;
    }
    if let Some(delivery) = params.get("delivery").and_then(Value::as_str) {
        delivery.clone_into(&mut rule.delivery);
    }
    if let Some(priority) = params.get("priority").and_then(Value::as_i64) {
        rule.priority = Some(priority);
    }
    settings.set_chunk_rule(&rule);
    Ok((Some(name), false))
}

/// The `limit` of a paged call (default [`PREVIEW_ROWS`], at most [`MAX_PAGE`]).
pub(crate) fn page_limit(params: &Value) -> Result<usize, RpcError> {
    match params.get("limit") {
        None => Ok(PREVIEW_ROWS),
        Some(value) => value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| (1..=MAX_PAGE).contains(n))
            .ok_or_else(|| RpcError::invalid_params(format!("`limit` must be 1 to {MAX_PAGE}"))),
    }
}

/// `editor.content_chunks_preview { platform?, chunk?, content?, include?, entry?, tags?, requires?,
/// delivery?, priority?, remove?, asset?, limit? }`: the assignment the edited rules would cook,
/// computed from `Regia.toml` with the edit applied in memory. Nothing is written.
fn content_chunks_preview(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(root) = crate::chunks_panel::project_root(world) else {
        return Err(rpc(ChunksPanelError {
            code: "content_chunks_no_project",
            message: "no project is open".into(),
        }));
    };
    let platform = params.get("platform").and_then(Value::as_str).unwrap_or("");
    let limit = page_limit(params)?;
    let mut settings = ProjectSettingsState::default();
    settings.load(&root).map_err(|message| {
        rpc(ChunksPanelError {
            code: "content_chunks_rules",
            message,
        })
    })?;
    let saved = settings.manifest_text();
    if params.get("chunk").is_some() || params.get("content").is_some() {
        apply_rule_params(&mut settings, params)?;
    }
    let edited = settings.manifest_text().unwrap_or_default();
    let fingerprint = inputs_fingerprint(&root, platform, &edited);
    let preview = compute_preview(&root, platform, saved.as_deref(), &edited, fingerprint).map_err(rpc)?;
    if inputs_fingerprint(&root, platform, &edited) != fingerprint {
        return Err(rpc(ChunksPanelError {
            code: "content_chunks_preview_stale",
            message:
                "Regia.toml, the cooked registry or a file under assets/ changed while the preview computed"
                    .into(),
        }));
    }
    let mut out = preview_json(&preview, limit);
    if let Some(asset) = params.get("asset").and_then(Value::as_str) {
        out["chain"] = json!(preview.reason_chain(asset));
    }
    Ok(out)
}

/// One page of unshipped assets: those after `cursor` (exclusive) in path order that `filter`
/// matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnshippedPage {
    /// Every unshipped asset.
    pub total: usize,
    /// Unshipped assets the filter matches.
    pub matched: usize,
    /// This page's paths.
    pub paths: Vec<String>,
    /// The cursor of the next page (`None` on the last page).
    pub next_cursor: Option<String>,
}

/// Whether an unshipped path matches `filter`: a glob under `assets/` when it holds `*` or `?`,
/// else a substring of the path.
#[must_use]
pub fn unshipped_filter_matches(filter: &str, path: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    if filter.contains(['*', '?']) {
        glob_match(filter, path.strip_prefix("assets/").unwrap_or(path))
    } else {
        path.contains(filter)
    }
}

/// Pages through `unshipped` (sorted, unique paths). Keyset paging: the cursor is the last path of
/// the previous page, so every matching path is on exactly one page even when the list is long.
#[must_use]
pub fn unshipped_page(
    unshipped: &[String],
    filter: &str,
    cursor: Option<&str>,
    limit: usize,
) -> UnshippedPage {
    let start = cursor.map_or(0, |c| unshipped.partition_point(|p| p.as_str() <= c));
    let matched = unshipped
        .iter()
        .filter(|p| unshipped_filter_matches(filter, p))
        .count();
    let mut rest = unshipped[start..]
        .iter()
        .filter(|p| unshipped_filter_matches(filter, p));
    let paths: Vec<String> = rest.by_ref().take(limit).cloned().collect();
    let next_cursor = if rest.next().is_some() {
        paths.last().cloned()
    } else {
        None
    };
    UnshippedPage {
        total: unshipped.len(),
        matched,
        paths,
        next_cursor,
    }
}

/// `editor.content_chunks_unshipped { platform?, filter?, cursor?, limit?, refresh? }`.
fn content_chunks_unshipped(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let limit = page_limit(params)?;
    let filter = params
        .get("filter")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let cursor = params.get("cursor").and_then(Value::as_str).map(str::to_owned);
    let mut state = json!({});
    for key in ["platform", "refresh"] {
        if let Some(v) = params.get(key) {
            state[key] = v.clone();
        }
    }
    let root = crate::chunks_panel::project_root(world);
    let mut panel = world
        .remove_resource::<crate::chunks_panel::ContentChunksPanel>()
        .unwrap_or_default();
    let result = crate::chunks_panel::drive_state(&mut panel, root.as_deref(), &state).map(|_| {
        let view = panel.view.clone().unwrap_or_default();
        let page = unshipped_page(&view.unshipped, &filter, cursor.as_deref(), limit);
        let assets: Vec<Value> = page
            .paths
            .iter()
            .map(|path| {
                let entry = view.registry.resolve(path);
                json!({
                    "path": path,
                    "type": entry.map(|e| e.type_path.rsplit("::").next().unwrap_or(&e.type_path).to_owned()),
                    "bytes": entry.map_or(0, |e| e.size),
                    "reason": "unreached",
                })
            })
            .collect();
        json!({
            "content": view.content.name(),
            "total": page.total,
            "matched": page.matched,
            "filter": filter,
            "assets": assets,
            "next_cursor": page.next_cursor,
        })
    });
    world.insert_resource(panel);
    result
}

const P_CHUNK: ParamSchema = ParamSchema {
    name: "chunk",
    kind: "string",
    required: false,
    description: "The chunk rule to edit (a new name adds a rule). Required unless `content` is given.",
};
const P_CONTENT: ParamSchema = ParamSchema {
    name: "content",
    kind: "string",
    required: false,
    description: "[cook.chunk_options] content: all (every asset ships) or closure (only what the roots reach).",
};
const P_INCLUDE: ParamSchema = ParamSchema {
    name: "include",
    kind: "array",
    required: false,
    description: "Globs under assets/.",
};
const P_ENTRY: ParamSchema = ParamSchema {
    name: "entry",
    kind: "array",
    required: false,
    description: "Entry-root globs under assets/: content loaded by name that ships with content = closure.",
};
const P_TAGS: ParamSchema = ParamSchema {
    name: "tags",
    kind: "array",
    required: false,
    description: "Registry tags, key or key=value.",
};
const P_REQUIRES: ParamSchema = ParamSchema {
    name: "requires",
    kind: "array",
    required: false,
    description: "Chunks this one may reference.",
};
const P_DELIVERY: ParamSchema = ParamSchema {
    name: "delivery",
    kind: "string",
    required: false,
    description: "install, fast_follow, on_demand or external; empty for the default.",
};
const P_PRIORITY: ParamSchema = ParamSchema {
    name: "priority",
    kind: "integer",
    required: false,
    description: "1 to 1000.",
};
const P_REMOVE: ParamSchema = ParamSchema {
    name: "remove",
    kind: "boolean",
    required: false,
    description: "Delete the rule.",
};

/// The closure settings' agent methods (the preview and the unshipped pages; the setter is
/// `editor.content_chunks_set_rule` in the Content Chunks panel).
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    const PREVIEW_PARAMS: &[ParamSchema] = &[
        P_CHUNK,
        P_CONTENT,
        P_INCLUDE,
        P_ENTRY,
        P_TAGS,
        P_REQUIRES,
        P_DELIVERY,
        P_PRIORITY,
        P_REMOVE,
        ParamSchema {
            name: "platform",
            kind: "string",
            required: false,
            description: "Cooked platform to read (default: the host).",
        },
        ParamSchema {
            name: "asset",
            kind: "string",
            required: false,
            description: "Also return this asset's reason chain back to the declaration that ships it.",
        },
        ParamSchema {
            name: "limit",
            kind: "integer",
            required: false,
            description: "Most rows of each list (default 100, at most 1000).",
        },
    ];
    vec![
        (
            MethodSchema {
                name: "editor.content_chunks_preview",
                tool: "content_chunks_preview",
                description: "Preview a content-chunk settings edit without saving: the same fields as \
                    editor.content_chunks_set_rule are applied to Regia.toml in memory and the assignment the next \
                    cook would make is returned (shipped and unshipped counts, chunk sizes in assets, the assets \
                    whose chunk changes, every declared root and whether it matched, and optionally one asset's \
                    reason chain). `valid: false` with an error_code means the edit would not save.",
                params: PREVIEW_PARAMS,
                result: "{ platform, content, valid, error_code, error, shipped, chunks: { name: count }, unshipped_total, unshipped, moves_total, moves: [{ path, from, to }], roots: [{ declared_by, pattern, matched }], unmatched, root_coverage, fingerprint, chain? }",
                mutating: false,
                permission: Permission::Read,
            },
            content_chunks_preview as Handler,
        ),
        (
            MethodSchema {
                name: "editor.content_chunks_unshipped",
                tool: "content_chunks_unshipped",
                description: "Page through the cooked assets that ship in no chunk (content = closure and no root \
                    reaches them), in path order, optionally filtered by a substring or a glob under assets/. Pass the \
                    returned next_cursor to get the next page; every matching asset is on exactly one page.",
                params: &[
                    ParamSchema {
                        name: "filter",
                        kind: "string",
                        required: false,
                        description: "A substring of the path, or a glob under assets/ when it holds * or ?.",
                    },
                    ParamSchema {
                        name: "cursor",
                        kind: "string",
                        required: false,
                        description: "next_cursor of the previous page.",
                    },
                    ParamSchema {
                        name: "limit",
                        kind: "integer",
                        required: false,
                        description: "Page size (default 100, at most 1000).",
                    },
                    ParamSchema {
                        name: "platform",
                        kind: "string",
                        required: false,
                        description: "Cooked platform to read (default: the host).",
                    },
                    ParamSchema {
                        name: "refresh",
                        kind: "boolean",
                        required: false,
                        description: "Recompute from Regia.toml and the cooked registry first.",
                    },
                ],
                result: "{ content, total, matched, filter, assets: [{ path, type, bytes, reason }], next_cursor }",
                mutating: false,
                permission: Permission::Read,
            },
            content_chunks_unshipped as Handler,
        ),
    ]
}

/// The parameters of `editor.content_chunks_set_rule` (shared with the preview).
pub(crate) const SET_RULE_PARAMS: &[ParamSchema] = &[
    P_CHUNK,
    P_CONTENT,
    P_INCLUDE,
    P_ENTRY,
    P_TAGS,
    P_REQUIRES,
    P_DELIVERY,
    P_PRIORITY,
    P_REMOVE,
    ParamSchema {
        name: "platform",
        kind: "string",
        required: false,
        description: "Cooked platform the entry roots are checked against (default: the host).",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyset_pages_visit_each_path_once() {
        let all: Vec<String> = (0..23).map(|i| format!("assets/a/{i:03}.png")).collect();
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let page = unshipped_page(&all, "", cursor.as_deref(), 5);
            assert_eq!(page.total, 23);
            seen.extend(page.paths.clone());
            pages += 1;
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(pages, 5);
        assert_eq!(seen, all);
        let page = unshipped_page(&all, "a/01?.png", None, 100);
        assert_eq!(page.matched, 10);
        assert_eq!(page.next_cursor, None);
    }

    #[test]
    fn fingerprint_is_stable_and_sees_edits() {
        let dir = regia_test_utils::unique_temp_dir("closure-fingerprint");
        std::fs::create_dir_all(dir.join("assets")).expect("assets");
        std::fs::write(dir.join("Regia.toml"), "[project]\nname = \"a\"\n").expect("manifest");
        let a = inputs_fingerprint(&dir, "", "x");
        assert_eq!(a, inputs_fingerprint(&dir, "", "x"));
        assert_ne!(a, inputs_fingerprint(&dir, "", "y"));
        std::fs::write(dir.join("assets/new.txt"), "n").expect("file");
        assert_ne!(a, inputs_fingerprint(&dir, "", "x"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
