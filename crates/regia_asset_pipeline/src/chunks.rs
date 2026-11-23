// SPDX-License-Identifier: MPL-2.0
//! Chunk rules, assignment and chunk paks (`docs/specs/40-content-chunks-dlc-asset-packs.md`
//! § 2.2-2.4).
//!
//! [`ChunkRules`] come from `Regia.toml` (`[cook.chunks.*]`, `[cook.chunk_options]`, and the old
//! `chunks = { base = ["**"] }` shorthand). [`assign`] maps every registry asset to a chunk with a
//! [`Reason`]; it is a pure function of the registry, the rules and the startup ids (ordered maps,
//! ids sorted, ties by smaller id). [`check_references`] lists references a chunk may not make.
//! [`write_chunks`] writes one deterministic pak per chunk, `content-manifest.ron` and
//! `chunk-map.bin`, keeping a chunk's pak untouched when its inputs did not change. [`write_dlc`]
//! cooks one chunk against a frozen base.
//!
//! With `[cook.chunk_options] content = "closure"` ([`ContentMode::Closure`], ADR-8092) the cook
//! ships only the dependency closure of the declared entry points (each rule's `entry` list and the
//! startup ids); `include` and `tags` then only place shipped assets, and everything else is in
//! [`Assignment::unshipped`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia_asset::content::{
    BASE_CHUNK, CONTENT_MANIFEST_FORMAT_VERSION, ChunkEntry, ChunkMap, ChunkWhy, ContentManifest, Delivery,
    RESUME_BLOCK, WhyReason, WhyRecord, block_hashes, hash_file, mount_order, pack_name, validate_chunk_name,
};
use regia_asset::registry::{AssetRegistry, TagFilter, path_glob_match, source_path};
use regia_asset::{AssetId, AssetManifest};
use regia_core::id::TypePathHash;
use serde::{Deserialize, Serialize};

use crate::closure_roots::{
    RootDecl, RootError, RootEvidence, RootInputs, RootKind, raw_universe, resolve_roots,
};
use crate::pak_writer::{
    PakWriteError, PakWriteOptions, read_cooked_from, write_pak_registry_sibling, write_pak_with,
};

/// Highest rule priority.
pub const MAX_PRIORITY: u32 = 1000;
/// Default number of Android asset packs before the cook errors (`[content] android_pack_limit`).
pub const DEFAULT_ANDROID_PACK_LIMIT: usize = 50;
/// Number of Android asset packs above which the cook warns.
pub const ANDROID_PACK_WARN: usize = 40;
/// Default largest fast-follow or on-demand pack in bytes (`[content] android_pack_max_size`). A
/// placeholder like [`DEFAULT_ANDROID_PACK_LIMIT`] (ADR-1585 b, ADR-7236): verify it against the
/// store's current published limits.
pub const DEFAULT_ANDROID_PACK_MAX_BYTES: u64 = 512 << 20;
/// Default largest sum of every chunk the store delivers (install, fast-follow and on-demand) in
/// bytes (`[content] android_total_max_size`). A placeholder (ADR-7236).
pub const DEFAULT_ANDROID_TOTAL_MAX_BYTES: u64 = 4 << 30;
/// Authored registry tag (`config/asset_tags.ron`) marking an asset's references optional: its
/// value is a comma-separated list of path globs (under `assets/`, as in chunk rules); a dependency
/// whose path matches one is an optional reference, and a `base` reference through it is a warning
/// instead of `chunk_base_reference` error (spec 40 § 2.2, ADR-7236).
pub const OPTIONAL_REFERENCE_TAG: &str = "optional_references";
/// File next to the paks recording each chunk's input hash (incremental chunk writes).
pub const CHUNK_INPUTS_FILE: &str = ".chunk-inputs.ron";
/// Suffix of a DLC's single-chunk manifest entry file (`<chunk>.chunk.ron`).
pub const CHUNK_ENTRY_SUFFIX: &str = regia_asset::content::CHUNK_ENTRY_SUFFIX;

/// Where a dependency claimed with equal priority by several chunks goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SharedPolicy {
    /// Into `base`.
    #[default]
    Base,
    /// Copied into every claiming chunk.
    Duplicate,
}

/// What the cook ships (`[cook.chunk_options] content`, ADR-8092).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ContentMode {
    /// `"all"`: every registry asset ships; an asset no rule or walk claims is in `base`.
    #[default]
    All,
    /// `"closure"`: only the dependency closure of the entry points (every rule's `entry` and the
    /// startup ids) ships. `include` and `tags` place shipped assets; an asset reached from no
    /// entry point is in no chunk ([`Assignment::unshipped`]).
    Closure,
}

impl ContentMode {
    /// The `Regia.toml` spelling: `all` or `closure`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Closure => "closure",
        }
    }

    /// Parses the `Regia.toml` spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "all" => Some(Self::All),
            "closure" => Some(Self::Closure),
            _ => None,
        }
    }
}

/// One `[cook.chunks.<name>]` table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkRule {
    /// Chunk name.
    pub name: String,
    /// Path globs under `assets/` (`*`, `?`, `**`).
    pub include: Vec<String>,
    /// Entry points: path globs under `assets/` that select assets as `include` does and are the
    /// roots of the shipped closure in [`ContentMode::Closure`]. Each must match at least one asset
    /// (`chunk_entry_missing`).
    pub entry: Vec<String>,
    /// Asset tags (`key` or `key=value`) that also select assets.
    pub tags: Vec<String>,
    /// 1..=1000, default 1.
    pub priority: u32,
    /// Delivery, default `install`.
    pub delivery: Delivery,
    /// Other chunks this chunk may reference (`base` implicit).
    pub requires: Vec<String>,
    /// Take dependencies along (default true).
    pub follow: bool,
    /// May replace ids of earlier chunks (patches).
    pub overrides: bool,
    /// Members and their dependencies never leave `base` (meaningful on `base`).
    pub pin: bool,
    /// lz4-compress this chunk's pak when the cook compresses (default true). `false` stores every
    /// entry raw, so the runtime reads it as a view of the mapping (ADR-7971).
    pub compress: bool,
}

impl ChunkRule {
    /// A rule with every key at its default.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            include: Vec::new(),
            entry: Vec::new(),
            tags: Vec::new(),
            priority: 1,
            delivery: Delivery::Install,
            requires: Vec::new(),
            follow: true,
            overrides: false,
            pin: false,
            compress: true,
        }
    }
}

/// The chunk rules of a project.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChunkRules {
    /// Rules by chunk name.
    pub rules: BTreeMap<String, ChunkRule>,
    /// `[cook.chunk_options] shared`.
    pub shared: SharedPolicy,
    /// `[cook.chunk_options] content`: every asset, or the closure of the entry points.
    pub content: ContentMode,
    /// `[content] android_pack_limit`.
    pub android_pack_limit: usize,
    /// `[content] android_pack_max_size` in bytes (0 = [`DEFAULT_ANDROID_PACK_MAX_BYTES`]).
    pub android_pack_max_bytes: u64,
    /// `[content] android_total_max_size` in bytes (0 = [`DEFAULT_ANDROID_TOTAL_MAX_BYTES`]).
    pub android_total_max_bytes: u64,
    /// `[content] base_url`: the static host of `external` chunks (`content-host.ron`).
    pub base_url: Option<String>,
    /// Unknown keys and other non-fatal notes from parsing.
    pub warnings: Vec<String>,
}

/// Why an asset is in its chunk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reason {
    /// Selected by the chunk's rule: `rule_index` counts its `include` globs, then its `tags`, then
    /// its `entry` globs.
    Rule {
        /// Chunk.
        chunk: String,
        /// Selector index.
        rule_index: usize,
    },
    /// Reached from a direct asset; `via` is the first referrer on the shortest path.
    Follows {
        /// The referrer.
        via: AssetId,
    },
    /// Claimed with equal priority by several chunks.
    Shared,
    /// Reachable from a pinned chunk or the startup level.
    Pinned,
    /// Selected by no rule.
    Default,
}

impl Reason {
    /// `rule`, `follows`, `shared`, `pinned`, `default`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Rule { .. } => "rule",
            Self::Follows { .. } => "follows",
            Self::Shared => "shared",
            Self::Pinned => "pinned",
            Self::Default => "default",
        }
    }
}

/// The result of [`assign`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Assignment {
    /// Each asset's chunk (with `shared = "duplicate"`, the first claiming chunk by name).
    pub chunk_of: BTreeMap<AssetId, String>,
    /// Why each asset is where it is.
    pub reason: BTreeMap<AssetId, Reason>,
    /// Assets copied into several chunks (`shared = "duplicate"`): every chunk holding a copy.
    pub duplicates: BTreeMap<AssetId, Vec<String>>,
    /// Registry assets that ship in no chunk: with [`ContentMode::Closure`], the ones no entry point
    /// reaches (empty in [`ContentMode::All`]).
    pub unshipped: BTreeSet<AssetId>,
}

impl Assignment {
    /// Every chunk name, `base` first, then by name.
    #[must_use]
    pub fn chunk_names(&self) -> Vec<String> {
        let mut names: BTreeSet<String> = self.chunk_of.values().cloned().collect();
        names.extend(self.duplicates.values().flatten().cloned());
        names.remove(BASE_CHUNK);
        std::iter::once(BASE_CHUNK.to_owned()).chain(names).collect()
    }

    /// Ids stored in the pak of `chunk` (duplicates included), sorted.
    #[must_use]
    pub fn members(&self, chunk: &str) -> Vec<AssetId> {
        let mut ids: BTreeSet<AssetId> = self
            .chunk_of
            .iter()
            .filter(|(_, c)| c.as_str() == chunk)
            .map(|(id, _)| *id)
            .collect();
        ids.extend(
            self.duplicates
                .iter()
                .filter(|(_, cs)| cs.iter().any(|c| c == chunk))
                .map(|(id, _)| *id),
        );
        ids.into_iter().collect()
    }

    /// Whether `chunk` holds `id` (as its home or a duplicate).
    #[must_use]
    pub fn holds(&self, chunk: &str, id: AssetId) -> bool {
        self.chunk_of.get(&id).is_some_and(|c| c == chunk)
            || self
                .duplicates
                .get(&id)
                .is_some_and(|cs| cs.iter().any(|c| c == chunk))
    }

    /// The referrer chain from `id` back to a direct asset (`id` first), following `Follows`.
    #[must_use]
    pub fn via_chain(&self, id: AssetId) -> Vec<AssetId> {
        let mut chain = vec![id];
        let mut seen = BTreeSet::from([id]);
        let mut at = id;
        while let Some(Reason::Follows { via }) = self.reason.get(&at) {
            if !seen.insert(*via) {
                break;
            }
            chain.push(*via);
            at = *via;
        }
        chain
    }
}

/// The runtime reason table (`chunk-why.bin`, ADR-1876) of an assignment: every assigned asset
/// with its registry path, chunk, reason and duplicate chunks, plus each rule's selectors in
/// `rule_index` order (its `include` globs, then `tag:<tag>`, then `entry:<glob>`). Unshipped
/// assets have no record. Deterministic: records sorted by id, chunks by name.
#[must_use]
pub fn why_table(registry: &AssetRegistry, a: &Assignment, rules: &ChunkRules) -> ChunkWhy {
    let chunks: Vec<String> = a
        .chunk_of
        .values()
        .chain(a.duplicates.values().flatten())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let index = |name: &str| {
        chunks
            .binary_search_by(|c| c.as_str().cmp(name))
            .ok()
            .and_then(|i| u32::try_from(i).ok())
            .unwrap_or(0)
    };
    let selectors = rules
        .rules
        .iter()
        .map(|(name, rule)| {
            let list = rule
                .include
                .iter()
                .cloned()
                .chain(rule.tags.iter().map(|t| format!("tag:{t}")))
                .chain(rule.entry.iter().map(|g| format!("entry:{g}")))
                .collect();
            (name.clone(), list)
        })
        .collect();
    let records = a
        .chunk_of
        .iter()
        .map(|(id, chunk)| WhyRecord {
            id: *id,
            path: registry
                .by_id(*id)
                .map_or_else(|| id.to_string(), |e| e.path.clone()),
            chunk: index(chunk.as_str()),
            reason: match a.reason.get(id) {
                Some(Reason::Rule { rule_index, .. }) => {
                    WhyReason::Rule(u32::try_from(*rule_index).unwrap_or(u32::MAX))
                }
                Some(Reason::Follows { via }) => WhyReason::Follows(*via),
                Some(Reason::Shared) => WhyReason::Shared,
                Some(Reason::Pinned) => WhyReason::Pinned,
                Some(Reason::Default) | None => WhyReason::Default,
            },
            duplicated_in: a
                .duplicates
                .get(id)
                .map(|cs| cs.iter().map(|c| index(c.as_str())).collect())
                .unwrap_or_default(),
        })
        .collect();
    ChunkWhy {
        chunks,
        selectors,
        records,
    }
}

/// One chunk's sizes and shape for `regia assets chunks` and the editor's Content Chunks panel.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ChunkSummary {
    /// Chunk name.
    pub name: String,
    /// Delivery (`base` is always `install`).
    pub delivery: Delivery,
    /// Assets stored in the chunk's pak (duplicates included).
    pub assets: usize,
    /// Pak file bytes (0 before the chunk paks are written).
    pub disk_bytes: u64,
    /// Decompressed cooked bytes of the members.
    pub memory_bytes: u64,
    /// Chunks it may reference besides `base`.
    pub requires: Vec<String>,
    /// Mount position.
    pub mount_order: u32,
    /// Memory bytes by asset type (the type path's last segment).
    pub by_type: BTreeMap<String, u64>,
}

/// Summaries of every chunk of `a`, in mount order (ties by name). Memory sizes come from the
/// registry, or from the cooked files in `cache_dir` when the registry has none; disk sizes from the
/// paks in `pak_dir`.
///
/// # Errors
/// A requirement cycle among the rules.
pub fn summarize_chunks(
    registry: &AssetRegistry,
    a: &Assignment,
    rules: &ChunkRules,
    cache_dir: &Path,
    pak_dir: &Path,
) -> Result<Vec<ChunkSummary>, ChunkError> {
    let order = rules.mount_order()?;
    let mut names = a.chunk_names();
    names.sort_by_key(|n| (order.get(n).copied().unwrap_or(u32::MAX), n.clone()));
    Ok(names
        .into_iter()
        .map(|name| {
            let members = a.members(&name);
            let rule = rules.rule(&name);
            let mut by_type: BTreeMap<String, u64> = BTreeMap::new();
            let mut memory = 0u64;
            for id in &members {
                if let Some(e) = registry.by_id(*id) {
                    let size = if e.size > 0 {
                        e.size
                    } else {
                        std::fs::metadata(cache_dir.join(regia_asset::cooked_file_name(*id)))
                            .map_or(0, |m| m.len())
                    };
                    memory += size;
                    let ty = e.type_path.rsplit("::").next().unwrap_or(&e.type_path).to_owned();
                    *by_type.entry(ty).or_default() += size;
                }
            }
            ChunkSummary {
                disk_bytes: std::fs::metadata(pak_dir.join(format!("{name}.fpak"))).map_or(0, |m| m.len()),
                delivery: if name == BASE_CHUNK {
                    Delivery::Install
                } else {
                    rule.delivery
                },
                assets: members.len(),
                memory_bytes: memory,
                requires: rule.requires.clone(),
                mount_order: order.get(&name).copied().unwrap_or(0),
                by_type,
                name,
            }
        })
        .collect())
}

/// Severity of a [`ChunkProblem`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// The cook continues.
    Warning,
    /// The cook fails.
    Error,
}

/// A reference a chunk may not make, or a rule problem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkProblem {
    /// Agent reason code (`chunk_base_reference`, `chunk_unlisted_reference`, `chunk_cycle`,
    /// `chunk_not_found`, `content_dependency_missing`).
    pub reason: &'static str,
    /// Error or warning.
    pub severity: Severity,
    /// The referring chunk.
    pub chunk: String,
    /// The referring asset (nil for rule problems).
    pub asset: AssetId,
    /// The referenced asset (nil for rule problems).
    pub target: AssetId,
    /// Human-readable text.
    pub message: String,
    /// Dependency-integrity detail (`unknown_registry_id`, `excluded_required_target`, or
    /// `missing_cooked_file`); `None` for chunk placement problems.
    pub detail: Option<&'static str>,
    /// Root-to-target registry paths (an unknown target is its ID); empty for placement problems.
    pub root_path: Vec<String>,
}

/// Completeness diagnostics from a bounded dependency walk, independent of advisory validation.
#[derive(Clone, Debug, Default)]
pub struct DependencyIntegrity {
    /// Missing required targets or cooked files, in deterministic traversal order.
    pub problems: Vec<ChunkProblem>,
    /// Number of registry assets expanded. Each shipped asset is expanded at most once.
    pub visited_assets: usize,
    /// Number of dependency edges inspected, including optional edges and cycle back edges.
    pub examined_edges: usize,
    /// Peak number of queued assets (each asset is enqueued once).
    pub peak_queue: usize,
    /// Number of predecessor records retained by the walk.
    pub tracked_assets: usize,
}

/// Checks the required edges of `shipped`, starting at `roots`, then covering any remaining
/// shipped components. Optional edges may leave `shipped` only when the existing registry tag
/// contract identifies the target. Unknown IDs cannot match a path-based optional declaration.
/// `available`, when supplied, contains IDs with a cooked file that the package can write.
///
/// This does not change assignment or the permissive graph walk. The queue and predecessor map
/// hold at most one entry per supplied root or shipped asset; cycles are legal. Diagnostic paths
/// are reconstructed only for failures. Callers must reject nonempty `problems` before publishing
/// content.
#[must_use]
pub fn check_dependency_integrity(
    registry: &AssetRegistry,
    roots: &[AssetId],
    shipped: &BTreeSet<AssetId>,
    optional: &BTreeSet<(AssetId, AssetId)>,
    available: Option<&BTreeSet<AssetId>>,
) -> DependencyIntegrity {
    let _span = tracing::info_span!(target: "regia_asset_pipeline", "content_dependency_integrity").entered();
    let index: Index<'_> = registry.entries().map(|e| (e.id, e)).collect();
    let mut report = DependencyIntegrity::default();
    let mut parents = std::collections::HashMap::<AssetId, Option<AssetId>>::new();
    let mut queue = std::collections::VecDeque::new();
    let path = |id: AssetId| index.get(&id).map_or_else(|| id.to_string(), |e| e.path.clone());
    // Seed one component at a time: a dependency that is also selected by a rule retains the
    // path from the entry point instead of becoming a second, shorter diagnostic root.
    for root in roots.iter().chain(shipped) {
        let std::collections::hash_map::Entry::Vacant(slot) = parents.entry(*root) else {
            continue;
        };
        slot.insert(None);
        queue.push_back(*root);
        report.peak_queue = report.peak_queue.max(queue.len());
        while let Some(id) = queue.pop_front() {
            let mut problem = |target: AssetId, detail: &'static str| {
                let mut chain = vec![id];
                let mut at = id;
                while let Some(Some(parent)) = parents.get(&at) {
                    chain.push(*parent);
                    at = *parent;
                }
                chain.reverse();
                if target != id {
                    chain.push(target);
                }
                let root_path: Vec<String> = chain.into_iter().map(&path).collect();
                report.problems.push(ChunkProblem {
                    reason: "content_dependency_missing",
                    severity: Severity::Error,
                    chunk: String::new(),
                    asset: id,
                    target,
                    message: format!(
                        "{detail}: {}{}",
                        root_path.join(" -> "),
                        if detail == "missing_cooked_file" {
                            format!(" (chunk_asset_unwritten: {target})")
                        } else {
                            String::new()
                        }
                    ),
                    detail: Some(detail),
                    root_path,
                });
            };
            let Some(entry) = index.get(&id) else {
                problem(id, "unknown_registry_id");
                continue;
            };
            if !shipped.contains(&id) {
                problem(id, "excluded_required_target");
                continue;
            }
            if available.is_some_and(|set| !set.contains(&id)) {
                problem(id, "missing_cooked_file");
            }
            report.visited_assets += 1;
            for dep in &entry.dependencies {
                report.examined_edges += 1;
                if !index.contains_key(dep) {
                    problem(*dep, "unknown_registry_id");
                } else if !shipped.contains(dep) && !optional.contains(&(id, *dep)) {
                    problem(
                        *dep,
                        if available.is_some_and(|set| !set.contains(dep)) {
                            "missing_cooked_file"
                        } else {
                            "excluded_required_target"
                        },
                    );
                }
            }
            for dep in &entry.dependencies {
                if shipped.contains(dep)
                    && index.contains_key(dep)
                    && let std::collections::hash_map::Entry::Vacant(slot) = parents.entry(*dep)
                {
                    slot.insert(Some(id));
                    queue.push_back(*dep);
                    report.peak_queue = report.peak_queue.max(queue.len());
                }
            }
        }
    }
    report.tracked_assets = parents.len();
    report
}

/// Completeness of a chunk assignment, retaining entry-point paths in closure mode and covering
/// every selected component in all-content mode. `available` has the same meaning as in
/// [`check_dependency_integrity`]. This is separate from permissive [`assign`].
#[must_use]
pub fn check_assignment_integrity(
    registry: &AssetRegistry,
    a: &Assignment,
    rules: &ChunkRules,
    optional: &BTreeSet<(AssetId, AssetId)>,
    available: Option<&BTreeSet<AssetId>>,
) -> DependencyIntegrity {
    let selectors: Vec<Selector<'_>> = rules.rules.values().map(Selector::new).collect();
    let mut roots = entry_points(registry, &selectors, None).unwrap_or_default();
    let shipped: BTreeSet<AssetId> = a.chunk_of.keys().copied().collect();
    let incoming: BTreeSet<AssetId> = registry
        .entries()
        .filter(|e| shipped.contains(&e.id))
        .flat_map(|e| e.dependencies.iter().copied())
        .collect();
    roots.extend(shipped.difference(&incoming));
    let mut report = check_dependency_integrity(
        registry,
        &roots.into_iter().collect::<Vec<_>>(),
        &shipped,
        optional,
        available,
    );
    for p in &mut report.problems {
        p.chunk = a.chunk_of.get(&p.asset).cloned().unwrap_or_default();
    }
    report
}

/// Errors of chunk rules, assignment and chunk writing.
#[derive(Debug, thiserror::Error)]
pub enum ChunkError {
    /// Two rules of equal priority select one asset.
    #[error("chunk rules {first:?} and {second:?} (priority {priority}) both select {asset}")]
    RuleConflict {
        /// The asset path.
        asset: String,
        /// First chunk.
        first: String,
        /// Second chunk.
        second: String,
        /// Their priority.
        priority: u32,
    },
    /// A rule or option is invalid.
    #[error("chunk rules: {0}")]
    BadRule(String),
    /// A requires cycle.
    #[error("chunk requirement cycle among {0:?}")]
    Cycle(Vec<String>),
    /// An `entry` glob of a rule matches no registry asset.
    #[error("chunk {chunk:?} entry {entry:?} matches no asset (a misspelled entry would ship nothing)")]
    EntryMissing {
        /// Chunk.
        chunk: String,
        /// The entry glob.
        entry: String,
    },
    /// `content = "closure"` without any entry point (no rule `entry`, no startup level).
    #[error(
        "[cook.chunk_options] content = \"closure\" ships the closure of the entry points, but no rule has an \
         entry and there is no startup level"
    )]
    NoEntry,
    /// A requirement names no chunk.
    #[error("chunk {chunk:?} requires unknown chunk {missing:?}")]
    NotFound {
        /// Chunk.
        chunk: String,
        /// The unknown name.
        missing: String,
    },
    /// A DLC asset id exists in the base and the DLC does not set `overrides`.
    #[error(
        "DLC chunk {chunk:?}: asset {asset} already exists in the base build (set overrides = true for a patch)"
    )]
    DlcIdCollision {
        /// Chunk.
        chunk: String,
        /// The colliding asset path.
        asset: String,
    },
    /// Two chunk names map to one Android pack name.
    #[error("chunks {0:?} and {1:?} map to the same Android asset pack name")]
    PackNameCollision(String, String),
    /// Too many Android asset packs.
    #[error("{count} Android asset packs exceed the limit {limit}: {chunks:?}")]
    PackLimit {
        /// Packs.
        count: usize,
        /// Limit.
        limit: usize,
        /// Offending chunks.
        chunks: Vec<String>,
    },
    /// Android store chunks over a size cap (`pack_limit`).
    #[error("Android {scope} size {bytes} bytes exceeds the limit {limit} bytes: {chunks:?}")]
    PackSize {
        /// `pack` (one fast-follow or on-demand pack) or `total` (every chunk the store delivers).
        scope: &'static str,
        /// The offending size in bytes.
        bytes: u64,
        /// The cap in bytes.
        limit: u64,
        /// Offending chunks (the pack, or every chunk counted in the total), sorted.
        chunks: Vec<String>,
    },
    /// Reference problems with severity error.
    #[error("{} chunk reference error(s); first: {}", .0.len(), .0.first().map_or("", |p| p.message.as_str()))]
    References(Vec<ChunkProblem>),
    /// Pak writing failed.
    #[error(transparent)]
    Pak(#[from] PakWriteError),
    /// Manifest or chunk-map writing failed.
    #[error("content: {0}")]
    Content(String),
    /// IO error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl ChunkError {
    /// The agent `data.reason` code.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::RuleConflict { .. } => "chunk_rule_conflict",
            Self::BadRule(_) => "bad_params",
            Self::Cycle(_) => "chunk_cycle",
            Self::EntryMissing { .. } => "chunk_entry_missing",
            Self::NoEntry => "chunk_no_entry",
            Self::NotFound { .. } => "chunk_not_found",
            Self::DlcIdCollision { .. } => "dlc_id_collision",
            Self::PackNameCollision(..) => "pack_name_collision",
            Self::PackLimit { .. } | Self::PackSize { .. } => "pack_limit",
            Self::References(problems) => problems.first().map_or("chunk_base_reference", |p| p.reason),
            Self::Pak(_) | Self::Content(_) | Self::Io(_) => "io",
        }
    }
}

impl ChunkRules {
    /// Rules from a whole `Regia.toml` text: `[cook] chunks`, `[cook.chunk_options]`, `[content]`.
    /// A manifest without chunk keys gives empty rules (everything in `base`).
    ///
    /// # Errors
    /// The TOML does not parse, or a rule is invalid ([`ChunkError::BadRule`]).
    pub fn from_manifest_toml(text: &str) -> Result<Self, ChunkError> {
        let value: toml::Table = text
            .parse()
            .map_err(|e| ChunkError::BadRule(format!("Regia.toml: {e}")))?;
        Self::from_table(&value)
    }

    /// Rules from the parsed `Regia.toml` root table.
    ///
    /// # Errors
    /// A rule is invalid.
    pub fn from_table(root: &toml::Table) -> Result<Self, ChunkError> {
        let mut rules = Self {
            android_pack_limit: DEFAULT_ANDROID_PACK_LIMIT,
            ..Self::default()
        };
        let cook = root.get("cook").and_then(toml::Value::as_table);
        if let Some(chunks) = cook.and_then(|c| c.get("chunks")) {
            let Some(chunks) = chunks.as_table() else {
                return Err(ChunkError::BadRule("[cook] chunks must be a table".into()));
            };
            for (name, value) in chunks {
                validate_chunk_name(name)
                    .map_err(|why| ChunkError::BadRule(format!("chunk name {name:?} {why}")))?;
                let rule = match value {
                    toml::Value::Array(globs) => {
                        let mut rule = ChunkRule::new(name);
                        rule.include = strings(globs, name, "include")?;
                        rule
                    }
                    toml::Value::Table(table) => parse_rule(name, table, &mut rules.warnings)?,
                    _ => {
                        return Err(ChunkError::BadRule(format!(
                            "[cook.chunks.{name}] must be a table or an array of globs"
                        )));
                    }
                };
                rules.rules.insert(name.clone(), rule);
            }
        }
        if let Some(options) = cook
            .and_then(|c| c.get("chunk_options"))
            .and_then(toml::Value::as_table)
        {
            for (key, value) in options {
                match (key.as_str(), value.as_str()) {
                    ("shared", Some("base")) => rules.shared = SharedPolicy::Base,
                    ("shared", Some("duplicate")) => rules.shared = SharedPolicy::Duplicate,
                    ("shared", _) => {
                        return Err(ChunkError::BadRule(
                            "[cook.chunk_options] shared must be \"base\" or \"duplicate\"".into(),
                        ));
                    }
                    ("content", text) => {
                        rules.content = text.and_then(ContentMode::parse).ok_or_else(|| {
                            ChunkError::BadRule(
                                "[cook.chunk_options] content must be \"all\" or \"closure\"".into(),
                            )
                        })?;
                    }
                    _ => rules
                        .warnings
                        .push(format!("unknown key [cook.chunk_options] {key}")),
                }
            }
        }
        if let Some(limit) = root
            .get("content")
            .and_then(toml::Value::as_table)
            .and_then(|c| c.get("android_pack_limit"))
        {
            rules.android_pack_limit = limit
                .as_integer()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| ChunkError::BadRule("[content] android_pack_limit must be a count".into()))?;
        }
        let content = root.get("content").and_then(toml::Value::as_table);
        if let Some(value) = content.and_then(|c| c.get("android_pack_max_size")) {
            rules.android_pack_max_bytes = content_size(value, "android_pack_max_size")?;
        }
        if let Some(value) = content.and_then(|c| c.get("android_total_max_size")) {
            rules.android_total_max_bytes = content_size(value, "android_total_max_size")?;
        }
        if let Some(url) = root
            .get("content")
            .and_then(toml::Value::as_table)
            .and_then(|c| c.get("base_url"))
        {
            let url = url
                .as_str()
                .ok_or_else(|| ChunkError::BadRule("[content] base_url must be a string".into()))?;
            regia_asset::content::HttpUrl::parse(url)
                .map_err(|why| ChunkError::BadRule(format!("[content] base_url: {why}")))?;
            rules.base_url = Some(url.to_owned());
        }
        for rule in rules.rules.values() {
            for req in &rule.requires {
                if req != BASE_CHUNK && !rules.rules.contains_key(req) {
                    return Err(ChunkError::NotFound {
                        chunk: rule.name.clone(),
                        missing: req.clone(),
                    });
                }
            }
        }
        rules.mount_order()?;
        Ok(rules)
    }

    /// Whether no rule is written (the whole project is `base`).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The rule of `chunk`, or the defaults.
    #[must_use]
    pub fn rule(&self, chunk: &str) -> ChunkRule {
        self.rules
            .get(chunk)
            .cloned()
            .unwrap_or_else(|| ChunkRule::new(chunk))
    }

    /// Mount positions of every rule's chunk plus `base`.
    ///
    /// # Errors
    /// [`ChunkError::Cycle`].
    pub fn mount_order(&self) -> Result<BTreeMap<String, u32>, ChunkError> {
        let mut graph: BTreeMap<String, Vec<String>> = self
            .rules
            .iter()
            .map(|(n, r)| (n.clone(), r.requires.clone()))
            .collect();
        graph.entry(BASE_CHUNK.to_owned()).or_default();
        mount_order(&graph).map_err(|e| match e {
            regia_asset::content::ContentError::ChunkCycle(names) => ChunkError::Cycle(names),
            other => ChunkError::BadRule(other.to_string()),
        })
    }

    /// Chunks a chunk may reference: itself, `base`, and its requirements, transitively.
    #[must_use]
    pub fn allowed_targets(&self, chunk: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::from([chunk.to_owned(), BASE_CHUNK.to_owned()]);
        let mut todo = vec![chunk.to_owned()];
        while let Some(at) = todo.pop() {
            for req in self
                .rules
                .get(&at)
                .map(|r| r.requires.clone())
                .unwrap_or_default()
            {
                if out.insert(req.clone()) {
                    todo.push(req);
                }
            }
        }
        out
    }
}

fn strings(values: &[toml::Value], chunk: &str, key: &str) -> Result<Vec<String>, ChunkError> {
    values
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| ChunkError::BadRule(format!("[cook.chunks.{chunk}] {key} must hold strings")))
        })
        .collect()
}

fn parse_rule(name: &str, table: &toml::Table, warnings: &mut Vec<String>) -> Result<ChunkRule, ChunkError> {
    let mut rule = ChunkRule::new(name);
    let bad = |what: &str| ChunkError::BadRule(format!("[cook.chunks.{name}] {what}"));
    for (key, value) in table {
        match key.as_str() {
            "include" | "entry" | "tags" | "requires" => {
                let list = value
                    .as_array()
                    .ok_or_else(|| bad(&format!("{key} must be an array")))?;
                let list = strings(list, name, key)?;
                match key.as_str() {
                    "include" => rule.include = list,
                    "entry" => rule.entry = list,
                    "tags" => {
                        for tag in &list {
                            TagFilter::parse(tag).map_err(|e| bad(&format!("tag {tag:?}: {e}")))?;
                        }
                        rule.tags = list;
                    }
                    _ => rule.requires = list,
                }
            }
            "priority" => {
                rule.priority = value
                    .as_integer()
                    .and_then(|n| u32::try_from(n).ok())
                    .filter(|n| (1..=MAX_PRIORITY).contains(n))
                    .ok_or_else(|| bad("priority must be 1..=1000"))?;
            }
            "delivery" => {
                rule.delivery = value
                    .as_str()
                    .and_then(Delivery::parse)
                    .ok_or_else(|| bad("delivery must be install, fast_follow, on_demand or external"))?;
            }
            "follow" | "overrides" | "pin" | "compress" => {
                let flag = value
                    .as_bool()
                    .ok_or_else(|| bad(&format!("{key} must be true or false")))?;
                match key.as_str() {
                    "follow" => rule.follow = flag,
                    "overrides" => rule.overrides = flag,
                    "compress" => rule.compress = flag,
                    _ => rule.pin = flag,
                }
            }
            other => warnings.push(format!("unknown key [cook.chunks.{name}] {other}")),
        }
    }
    if rule.pin && name != BASE_CHUNK {
        warnings.push(format!(
            "[cook.chunks.{name}] pin: its members and their dependencies are kept in base"
        ));
    }
    Ok(rule)
}

/// The path a chunk glob sees: the registry path without `assets/`.
fn rule_path(path: &str) -> &str {
    path.strip_prefix("assets/").unwrap_or(path)
}

/// Whether `glob` can match `path` at all: the path must start with the glob's literal prefix (the
/// text before its first wildcard), or be the directory a trailing `/**` names. A cheap filter in
/// front of [`path_glob_match`], which is exact but costs a dynamic program per call.
fn glob_may_match(glob: &str, path: &str) -> bool {
    let literal = glob.find(['*', '?']).map_or(glob, |i| &glob[..i]);
    path.starts_with(literal) || glob.strip_suffix("/**") == Some(path)
}

/// A rule with its tag filters parsed once.
struct Selector<'a> {
    rule: &'a ChunkRule,
    tags: Vec<TagFilter>,
}

impl<'a> Selector<'a> {
    fn new(rule: &'a ChunkRule) -> Self {
        Self {
            rule,
            tags: rule
                .tags
                .iter()
                .filter_map(|t| TagFilter::parse(t).ok())
                .collect(),
        }
    }

    /// The selector index that picks `entry`, if any: an `entry` glob first (an entry point is
    /// the more specific reason), then the include globs, then the tags. Indices count include
    /// globs, then tags, then entry globs.
    fn pick(&self, entry: &regia_asset::registry::RegistryEntry) -> Option<usize> {
        let path = rule_path(&entry.path);
        let source = rule_path(source_path(&entry.path));
        if let Some(i) = self.entry_index(path, source) {
            return Some(self.rule.include.len() + self.rule.tags.len() + i);
        }
        if let Some(i) = self.rule.include.iter().position(|g| glob_hits(g, path, source)) {
            return Some(i);
        }
        self.tags
            .iter()
            .position(|f| match f {
                TagFilter::Has(k) => entry.tags.contains_key(k),
                TagFilter::Equals(k, v) => entry.tags.get(k) == Some(v),
            })
            .map(|i| self.rule.include.len() + i)
    }

    /// The first `entry` glob matching `path` (or its `source`), if any.
    fn entry_index(&self, path: &str, source: &str) -> Option<usize> {
        self.rule.entry.iter().position(|g| glob_hits(g, path, source))
    }
}

/// Whether `glob` matches a rule path or its source path.
fn glob_hits(glob: &str, path: &str, source: &str) -> bool {
    (glob_may_match(glob, path) && path_glob_match(glob, path))
        || (source != path && glob_may_match(glob, source) && path_glob_match(glob, source))
}

/// The entry points of `rules` in `registry` (every asset an `entry` glob matches).
///
/// # Errors
/// [`ChunkError::EntryMissing`] for the first glob (by chunk name, then list order) that matches
/// nothing.
fn entry_points(
    registry: &AssetRegistry,
    selectors: &[Selector<'_>],
    matched: Option<&BTreeSet<String>>,
) -> Result<BTreeSet<AssetId>, ChunkError> {
    let mut out = BTreeSet::new();
    for selector in selectors.iter().filter(|s| !s.rule.entry.is_empty()) {
        let mut hit = vec![false; selector.rule.entry.len()];
        for entry in registry.entries() {
            let path = rule_path(&entry.path);
            let source = rule_path(source_path(&entry.path));
            for (i, glob) in selector.rule.entry.iter().enumerate() {
                if glob_hits(glob, path, source) {
                    hit[i] = true;
                    out.insert(entry.id);
                }
            }
        }
        // `None`: report every match, never fail (the integrity pass). A glob the root resolver
        // matched against raw files only (`matched`) is satisfied.
        let raw_hit = |i: usize| {
            matched.is_none_or(|m| m.contains(&format!("[cook.chunks.{}] entry[{i}]", selector.rule.name)))
        };
        if let Some(i) = hit.iter().enumerate().position(|(i, h)| !h && !raw_hit(i)) {
            return Err(ChunkError::EntryMissing {
                chunk: selector.rule.name.clone(),
                entry: selector.rule.entry[i].clone(),
            });
        }
    }
    Ok(out)
}

/// Registry records by id (one hash lookup per edge in the walks).
type Index<'a> = std::collections::HashMap<AssetId, &'a regia_asset::registry::RegistryEntry>;

/// Breadth-first closure of `start` over the registry's dependency lists, not entering `stop`.
/// Returns each reached id with the first referrer on its shortest path (ties by smaller id).
fn closure(
    registry: &Index<'_>,
    start: &BTreeSet<AssetId>,
    stop: &dyn Fn(AssetId) -> bool,
) -> BTreeMap<AssetId, AssetId> {
    let mut via: BTreeMap<AssetId, AssetId> = BTreeMap::new();
    let mut seen: BTreeSet<AssetId> = start.clone();
    let mut frontier: Vec<AssetId> = start.iter().copied().collect();
    while !frontier.is_empty() {
        frontier.sort();
        let mut next = Vec::new();
        for at in frontier {
            let Some(entry) = registry.get(&at) else {
                continue;
            };
            let mut deps = entry.dependencies.clone();
            deps.sort();
            for dep in deps {
                if !registry.contains_key(&dep) || stop(dep) || !seen.insert(dep) {
                    continue;
                }
                via.insert(dep, at);
                next.push(dep);
            }
        }
        frontier = next;
    }
    via
}

/// The `[project] startup_level` of a project's `Regia.toml`, if it names one.
fn startup_level(project: &Path) -> Option<String> {
    regia_core::brand::project_manifest(project)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| startup_level_in(&text))
}

/// `[project] startup_level` from a manifest's text. An empty value names no level: the editor writes
/// `""` for "none" and several samples ship it, so it must not become a declared root.
fn startup_level_in(text: &str) -> Option<String> {
    let table = text.parse::<toml::Table>().ok()?;
    let level = table.get("project")?.get("startup_level")?.as_str()?;
    (!level.trim().is_empty()).then(|| level.to_owned())
}

/// The root declarations of a chunk cook: `[project] startup_level` and every rule's `entry` glob
/// (`[cook.chunks.<name>] entry[<i>]`), the names [`RootEvidence`] reports and
/// [`assign_with_roots`] matches.
#[must_use]
pub fn chunk_root_decls(startup_level: Option<&str>, rules: &ChunkRules) -> Vec<RootDecl> {
    let mut decls: Vec<RootDecl> = startup_level
        .map(|level| RootDecl::new(RootKind::StartupLevel, "[project] startup_level", level))
        .into_iter()
        .collect();
    for (name, rule) in &rules.rules {
        for (i, glob) in rule.entry.iter().enumerate() {
            decls.push(RootDecl::new(
                RootKind::Declared,
                format!("[cook.chunks.{name}] entry[{i}]"),
                glob.clone(),
            ));
        }
    }
    decls
}

/// The root evidence of a chunk cook (CT-25, ADR-8477): the shared resolver
/// ([`crate::closure_roots::resolve_roots`], which named build profiles call too) over the
/// project's `startup_level`, the startup UI layouts, the levels' bakes and every `entry` glob.
/// [`RootEvidence::boot_ids`] are the startup ids [`assign`] pins in base; with
/// [`ContentMode::Closure`] every root is an entry point ([`assign_with_roots`]).
///
/// # Errors
/// [`RootError`] for a startup level that does not exist, an unreadable raw file, or a malformed
/// level or UI layout.
pub fn startup_roots(
    project: &Path,
    registry: &AssetRegistry,
    rules: &ChunkRules,
) -> Result<RootEvidence, RootError> {
    let raw = raw_universe(project)?;
    let cooked: Vec<(AssetId, String)> = registry.entries().map(|e| (e.id, e.path.clone())).collect();
    let inputs = RootInputs {
        assets_dir: project.join("assets"),
        raw: &raw,
        cooked: &cooked,
        exclude: &|_| false,
    };
    resolve_roots(
        &inputs,
        &chunk_root_decls(startup_level(project).as_deref(), rules),
        true,
    )
}

/// The cooked assets the project boots with (spec 40 § 2.2 step 4): the startup ids [`assign`]
/// pins in base, [`startup_roots`]' boot roots (the startup level, the files and bakes it needs,
/// the startup UI layouts and their pictures; levels it names are travel targets, not boot needs).
/// An analysis helper: a resolution error is logged and yields no ids. `regia cook` and
/// `regia package` call [`startup_roots`], which fails instead.
#[must_use]
pub fn startup_assets(project: &Path, registry: &AssetRegistry) -> Vec<AssetId> {
    match startup_roots(project, registry, &ChunkRules::default()) {
        Ok(evidence) => evidence.boot_ids(),
        Err(error) => {
            tracing::warn!(target: "regia_asset_pipeline", %error, "startup roots unresolved");
            Vec::new()
        }
    }
}

/// Assigns every registry asset to a chunk (spec 40 § 2.2 steps 1-5). With
/// [`ContentMode::Closure`] only the closure of the entry points (every rule's `entry` matches and
/// the startup ids) is assigned; the rest is [`Assignment::unshipped`]. This analysis API remains
/// permissive about dangling edges; use [`check_assignment_integrity`] before shipping.
///
/// # Errors
/// [`ChunkError::RuleConflict`] when two rules of equal priority select one asset,
/// [`ChunkError::EntryMissing`] when an `entry` glob matches no asset, [`ChunkError::NoEntry`] in
/// closure mode without any entry point.
pub fn assign(
    registry: &AssetRegistry,
    rules: &ChunkRules,
    startup: &[AssetId],
) -> Result<Assignment, ChunkError> {
    assign_inner(registry, rules, startup, &[], &BTreeSet::new())
}

/// [`assign`] with the shared root evidence of [`startup_roots`] (CT-25, ADR-8477): its boot
/// roots are the startup ids, and with [`ContentMode::Closure`] every root (travel-target levels'
/// content and the raw files an `entry` glob declares) is an entry point too. An `entry` glob that
/// matched only raw files (a level loaded by name) is not `chunk_entry_missing`.
///
/// # Errors
/// As [`assign`].
pub fn assign_with_roots(
    registry: &AssetRegistry,
    rules: &ChunkRules,
    roots: &RootEvidence,
) -> Result<Assignment, ChunkError> {
    assign_inner(
        registry,
        rules,
        &roots.boot_ids(),
        &roots.cooked_ids(),
        &roots.matched,
    )
}

fn assign_inner(
    registry: &AssetRegistry,
    rules: &ChunkRules,
    startup: &[AssetId],
    reachable: &[AssetId],
    matched: &BTreeSet<String>,
) -> Result<Assignment, ChunkError> {
    let _span =
        tracing::info_span!(target: "regia_asset_pipeline", "content_assign", assets = registry.len())
            .entered();
    let mut out = Assignment::default();
    let mut direct: BTreeMap<AssetId, (String, u32)> = BTreeMap::new();
    let selectors: Vec<Selector<'_>> = rules.rules.values().map(Selector::new).collect();
    let index: Index<'_> = registry.entries().map(|e| (e.id, e)).collect();
    let entries = entry_points(registry, &selectors, Some(matched))?;
    // 0. Closure mode: the shipped set is the entry points, the startup ids and everything they
    // reach (ADR-8092); nothing outside it is selected, walked or defaulted into base.
    let shipped: Option<BTreeSet<AssetId>> = match rules.content {
        ContentMode::All => None,
        ContentMode::Closure => {
            let mut roots = entries;
            roots.extend(startup.iter().copied().filter(|id| index.contains_key(id)));
            roots.extend(reachable.iter().copied().filter(|id| index.contains_key(id)));
            if roots.is_empty() {
                return Err(ChunkError::NoEntry);
            }
            let reach = closure(&index, &roots, &|_| false);
            roots.extend(reach.into_keys());
            Some(roots)
        }
    };
    let ships = |id: AssetId| shipped.as_ref().is_none_or(|s| s.contains(&id));
    // 1. Direct assets: the highest-priority rule that selects each.
    for entry in registry.entries() {
        if !ships(entry.id) {
            out.unshipped.insert(entry.id);
            continue;
        }
        let mut best: Option<(u32, &ChunkRule, usize)> = None;
        for selector in &selectors {
            let rule = selector.rule;
            let Some(index) = selector.pick(entry) else {
                continue;
            };
            match best {
                Some((p, other, _)) if p == rule.priority && other.name != rule.name => {
                    return Err(ChunkError::RuleConflict {
                        asset: entry.path.clone(),
                        first: other.name.clone(),
                        second: rule.name.clone(),
                        priority: p,
                    });
                }
                Some((p, ..)) if p >= rule.priority => {}
                _ => best = Some((rule.priority, rule, index)),
            }
        }
        if let Some((priority, rule, index)) = best {
            direct.insert(entry.id, (rule.name.clone(), priority));
            out.chunk_of.insert(entry.id, rule.name.clone());
            out.reason.insert(
                entry.id,
                Reason::Rule {
                    chunk: rule.name.clone(),
                    rule_index: index,
                },
            );
        }
    }
    // 3. Dependencies: every following chunk walks from its direct assets (not entering other
    // direct assets); the highest-priority claim wins, equal top claims are shared.
    let mut by_chunk: BTreeMap<String, BTreeSet<AssetId>> = BTreeMap::new();
    for (id, (chunk, _)) in &direct {
        by_chunk.entry(chunk.clone()).or_default().insert(*id);
    }
    let mut claims: BTreeMap<AssetId, Vec<(u32, String, AssetId)>> = BTreeMap::new();
    for (chunk, members) in &by_chunk {
        let rule = rules.rule(chunk);
        if !rule.follow {
            continue;
        }
        for (id, via) in closure(&index, members, &|id| direct.contains_key(&id)) {
            claims
                .entry(id)
                .or_default()
                .push((rule.priority, chunk.clone(), via));
        }
    }
    for (id, mut list) in claims {
        let top = list.iter().map(|(p, ..)| *p).max().unwrap_or(1);
        list.retain(|(p, ..)| *p == top);
        list.sort_by(|a, b| (&a.1, a.2).cmp(&(&b.1, b.2)));
        let chunks: BTreeSet<&String> = list.iter().map(|(_, c, _)| c).collect();
        if chunks.len() == 1 {
            let (_, chunk, via) = &list[0];
            out.chunk_of.insert(id, chunk.clone());
            out.reason.insert(id, Reason::Follows { via: *via });
        } else {
            out.reason.insert(id, Reason::Shared);
            match rules.shared {
                SharedPolicy::Base => {
                    out.chunk_of.insert(id, BASE_CHUNK.to_owned());
                }
                SharedPolicy::Duplicate => {
                    let names: Vec<String> = chunks.into_iter().cloned().collect();
                    out.chunk_of.insert(id, names[0].clone());
                    out.duplicates.insert(id, names);
                }
            }
        }
    }
    // 2. Assets selected by no rule and claimed by no walk are base members (in closure mode, the
    // shipped ones only).
    for entry in registry.entries() {
        if out.unshipped.contains(&entry.id) {
            continue;
        }
        if let std::collections::btree_map::Entry::Vacant(slot) = out.chunk_of.entry(entry.id) {
            slot.insert(BASE_CHUNK.to_owned());
            out.reason.insert(entry.id, Reason::Default);
        }
    }
    // 4. Everything reachable from a pinned chunk, and the startup ids with their closure, is in
    // base whatever claimed it (a base rule keeps its own reason).
    let mut pinned: BTreeSet<AssetId> = startup
        .iter()
        .copied()
        .filter(|id| registry.by_id(*id).is_some())
        .collect();
    for (chunk, members) in &by_chunk {
        if rules.rule(chunk).pin {
            pinned.extend(members.iter().copied());
        }
    }
    let reach = closure(&index, &pinned, &|_| false);
    for id in pinned.iter().copied().chain(reach.keys().copied()) {
        if matches!(out.reason.get(&id), Some(Reason::Rule { chunk, .. }) if chunk == BASE_CHUNK) {
            continue;
        }
        out.chunk_of.insert(id, BASE_CHUNK.to_owned());
        out.duplicates.remove(&id);
        out.reason.insert(id, Reason::Pinned);
    }
    Ok(out)
}

/// References a chunk may not make (spec 40 § 2.2 "Reference checks"), sorted by chunk, asset and
/// target. A chunk may reference itself, `base`, and its requirements transitively. Every `base`
/// reference into another chunk is an error here; [`check_references_with`] takes the optional
/// references that are warnings instead.
#[must_use]
pub fn check_references(registry: &AssetRegistry, a: &Assignment, rules: &ChunkRules) -> Vec<ChunkProblem> {
    check_references_with(registry, a, rules, &BTreeSet::new())
}

/// The optional references of a registry: `(referrer, dependency)` pairs where the referrer's
/// [`OPTIONAL_REFERENCE_TAG`] lists a path glob matching the dependency's path (or its source path).
/// The registry's dependency lists do not say which field a reference came from, so the mark is
/// authored per referring asset (`config/asset_tags.ron`) until field schemas reach the registry
/// (ADR-7236).
#[must_use]
pub fn optional_references(registry: &AssetRegistry) -> BTreeSet<(AssetId, AssetId)> {
    let mut out = BTreeSet::new();
    for entry in registry.entries() {
        let Some(value) = entry.tags.get(OPTIONAL_REFERENCE_TAG) else {
            continue;
        };
        let globs: Vec<&str> = value
            .split(',')
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .collect();
        for dep in &entry.dependencies {
            let Some(target) = registry.by_id(*dep) else {
                continue;
            };
            let path = rule_path(&target.path);
            let source = rule_path(source_path(&target.path));
            if globs
                .iter()
                .any(|g| path_glob_match(g, path) || (source != path && path_glob_match(g, source)))
            {
                out.insert((entry.id, *dep));
            }
        }
    }
    out
}

/// [`check_references`] where a `base` reference listed in `optional` (`(referrer, dependency)`,
/// from [`optional_references`]) is a [`Severity::Warning`] (`chunk_base_reference`): the runtime
/// treats the missing target as unavailable (spec 40 § 2.2, § 2.5). Chunk-to-unlisted-chunk
/// references stay errors.
#[must_use]
pub fn check_references_with(
    registry: &AssetRegistry,
    a: &Assignment,
    rules: &ChunkRules,
    optional: &BTreeSet<(AssetId, AssetId)>,
) -> Vec<ChunkProblem> {
    let mut problems = check_assignment_integrity(registry, a, rules, optional, None).problems;
    if let Err(ChunkError::Cycle(names)) = rules.mount_order() {
        problems.push(ChunkProblem {
            reason: "chunk_cycle",
            severity: Severity::Error,
            chunk: names.first().cloned().unwrap_or_default(),
            asset: AssetId::NIL,
            target: AssetId::NIL,
            message: format!("chunk requirement cycle among {names:?}"),
            detail: None,
            root_path: Vec::new(),
        });
        return problems;
    }
    let path = |id: AssetId| {
        registry
            .by_id(id)
            .map_or_else(|| id.to_string(), |e| e.path.clone())
    };
    for (id, chunk) in &a.chunk_of {
        let Some(entry) = registry.by_id(*id) else {
            continue;
        };
        let holders: Vec<&String> = a
            .duplicates
            .get(id)
            .map_or_else(|| vec![chunk], |cs| cs.iter().collect());
        for holder in holders {
            let allowed = rules.allowed_targets(holder);
            for dep in &entry.dependencies {
                let Some(target) = a.chunk_of.get(dep) else {
                    continue;
                };
                if a.holds(holder, *dep) || allowed.contains(target) {
                    continue;
                }
                let optional_ref = holder == BASE_CHUNK && optional.contains(&(*id, *dep));
                let (reason, message) = if optional_ref {
                    (
                        "chunk_base_reference",
                        format!(
                            "base asset {} references {} in chunk {target:?} through an optional reference; \
                             it reads as unavailable until {target:?} is mounted",
                            entry.path,
                            path(*dep)
                        ),
                    )
                } else if holder == BASE_CHUNK {
                    (
                        "chunk_base_reference",
                        format!(
                            "base asset {} references {} in chunk {target:?}; the base game would break without it",
                            entry.path,
                            path(*dep)
                        ),
                    )
                } else {
                    (
                        "chunk_unlisted_reference",
                        format!(
                            "{} in chunk {holder:?} references {} in chunk {target:?}, which {holder:?} does not list in requires",
                            entry.path,
                            path(*dep)
                        ),
                    )
                };
                problems.push(ChunkProblem {
                    reason,
                    severity: if optional_ref {
                        Severity::Warning
                    } else {
                        Severity::Error
                    },
                    chunk: holder.clone(),
                    asset: *id,
                    target: *dep,
                    message,
                    detail: None,
                    root_path: Vec::new(),
                });
            }
        }
    }
    problems.sort_by(|x, y| (&x.chunk, x.asset, x.target).cmp(&(&y.chunk, y.asset, y.target)));
    problems
}

/// Options of [`write_chunks`].
#[derive(Clone, Debug, Default)]
pub struct ChunkWriteOptions {
    /// lz4-compress pak entries (a chunk whose rule says `compress = false` is written raw).
    pub compress: bool,
    /// `PakWriteOptions::min_ratio` of every compressed chunk: an entry lz4 saves less than this
    /// fraction of is stored raw (`0.0`: any saving keeps lz4; `regia cook --ship` uses
    /// `pak_writer::SHIP_MIN_RATIO`).
    pub min_ratio: f32,
    /// Store desktop meshes GPU-ready (`pak_writer::gpu_meshes_for`).
    pub gpu_meshes: bool,
    /// Build id recorded in the manifest.
    pub build_id: String,
    /// Platform recorded in the manifest.
    pub platform: String,
    /// Other base builds this build accepts.
    pub compatible_with: Vec<String>,
}

/// What [`write_chunks`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChunkWriteReport {
    /// Chunks whose pak was (re)written.
    pub written: Vec<String>,
    /// Chunks whose pak was kept byte for byte (inputs unchanged).
    pub kept: Vec<String>,
    /// The pak files, in manifest order.
    pub paks: Vec<PathBuf>,
    /// Assets the assignment puts in a chunk that have no cooked file: in no pak, and left out of
    /// `chunk-map.bin` (audit v0.2.12). The CLI prints them as a warning.
    pub unwritten: Vec<AssetId>,
    /// Chunks the previous write made that the assignment no longer has: their pak, registry and
    /// entry files were removed from the output folder.
    pub removed: Vec<String>,
}

/// A chunk name read back from `chunk-inputs` that is a plain file stem (so a tampered file cannot
/// make the cleanup leave the output folder).
fn safe_chunk_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.')
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ChunkInputs {
    format_version: u32,
    /// chunk -> (input hash, pak hash)
    chunks: BTreeMap<String, (String, String)>,
}

/// Identity of the code that writes the paks: the pipeline's version and the pak format version.
const PAK_WRITER_STAMP: &str = concat!("regia_asset_pipeline:", env!("CARGO_PKG_VERSION"), ":fpak1");

/// Hash of what a chunk's pak is made from: member ids, their cooked bytes, the TOC manifest and
/// the write options (`compress` is the chunk's effective choice: the cook's and its rule's).
fn input_hash(
    cache: &Path,
    ids: &[AssetId],
    toc: &AssetManifest,
    (opts, compress): (&ChunkWriteOptions, bool),
    deps: &[String],
) -> Result<String, ChunkError> {
    let mut hasher = blake3::Hasher::new();
    // The writer: a pipeline or pak format change (the container, the compact-to-GPU mesh
    // conversion) must not keep a pak an older writer made (audit F6).
    hasher.update(PAK_WRITER_STAMP.as_bytes());
    hasher.update(&[u8::from(compress), u8::from(opts.gpu_meshes)]);
    // The ratio only changes a compressed pak, and a zero ratio hashes as before (ADR-7971).
    if compress && opts.min_ratio != 0.0 {
        hasher.update(b"min_ratio");
        hasher.update(&opts.min_ratio.to_le_bytes());
    }
    for id in ids {
        hasher.update(id.to_string().as_bytes());
        let file = cache.join(regia_asset::cooked_file_name(*id));
        match std::fs::read(&file) {
            Ok(bytes) => {
                hasher.update(blake3::hash(&bytes).as_bytes());
            }
            Err(e) => {
                return Err(ChunkError::Pak(PakWriteError::MissingCooked {
                    id: *id,
                    path: file,
                    source: e,
                }));
            }
        }
    }
    hasher.update(&postcard::to_allocvec(&toc.entries).map_err(|e| ChunkError::Content(e.to_string()))?);
    hasher.update(deps.join("\n").as_bytes());
    Ok(hasher.finalize().to_hex().to_string())
}

/// Writes one pak per chunk (`<chunk>.fpak`, entries in id order), `content-manifest.ron` and
/// `chunk-map.bin` into `out`. A chunk whose inputs hash and pak hash match the previous write is
/// kept untouched. With only `base` (no rules), `base.fpak` is byte-identical to
/// [`crate::pack_cooked_directory`]'s output. This low-level writer also supports partial/DLC
/// assignments; project publishers must run [`check_assignment_integrity`] before calling it.
///
/// # Errors
/// A cooked file is missing, a pak or the manifest cannot be written, or the rules have a cycle.
pub fn write_chunks(
    cache: &Path,
    out: &Path,
    a: &Assignment,
    rules: &ChunkRules,
    opts: &ChunkWriteOptions,
) -> Result<(ContentManifest, ChunkWriteReport), ChunkError> {
    std::fs::create_dir_all(out)?;
    let cache_manifest = AssetManifest::read_from_cache(cache)
        .map_err(|e| ChunkError::Content(e.to_string()))?
        .unwrap_or_default();
    let type_of: BTreeMap<AssetId, &regia_asset::ManifestEntry> =
        cache_manifest.entries.iter().map(|e| (e.id, e)).collect();
    let order = rules.mount_order()?;
    let inputs_path = out.join(CHUNK_INPUTS_FILE);
    let previous: ChunkInputs = std::fs::read_to_string(&inputs_path)
        .ok()
        .and_then(|t| ron::from_str(&t).ok())
        .unwrap_or_default();
    let mut inputs = ChunkInputs {
        format_version: 1,
        chunks: BTreeMap::new(),
    };
    // One base chunk holding every cooked asset writes today's single pak (its manifest is the whole
    // cache's). With unshipped assets (closure content, ADR-8092) the pak lists its members only.
    let only_base = a.chunk_names().len() == 1 && a.unshipped.is_empty();
    let mut manifest = ContentManifest {
        format_version: CONTENT_MANIFEST_FORMAT_VERSION,
        build_id: opts.build_id.clone(),
        platform: opts.platform.clone(),
        compatible_with: opts.compatible_with.clone(),
        chunks: Vec::new(),
    };
    let mut report = ChunkWriteReport::default();
    let mut names = a.chunk_names();
    names.sort_by_key(|n| (order.get(n).copied().unwrap_or(u32::MAX), n.clone()));
    for name in names {
        let _span = tracing::info_span!(target: "regia_asset_pipeline", "content_write_chunk", chunk = %name)
            .entered();
        let rule = rules.rule(&name);
        // Members whose cooked file exists (as `pack_cooked_directory` does).
        let ids: Vec<AssetId> = a
            .members(&name)
            .into_iter()
            .filter(|id| type_of.contains_key(id) && cache.join(regia_asset::cooked_file_name(*id)).is_file())
            .collect();
        let toc = if only_base {
            cache_manifest.clone()
        } else {
            AssetManifest {
                entries: cache_manifest
                    .entries
                    .iter()
                    .filter(|e| ids.binary_search(&e.id).is_ok())
                    .cloned()
                    .collect(),
            }
        };
        let deps: Vec<String> = rule
            .requires
            .iter()
            .filter(|r| r.as_str() != BASE_CHUNK)
            .map(|r| format!("{r}.fpak"))
            .collect();
        let file = format!("{name}.fpak");
        let pak = out.join(&file);
        // `[cook.chunks.<name>] compress = false` stores this chunk raw (ADR-7971).
        let compress = opts.compress && rule.compress;
        let input = input_hash(cache, &ids, &toc, (opts, compress), &deps)?;
        let kept = previous
            .chunks
            .get(&name)
            .is_some_and(|(inp, pak_hash)| *inp == input && hash_file(&pak).is_ok_and(|h| h == *pak_hash));
        if kept {
            // Same bytes, new modification time: tools that order the paks against the cook's
            // database (`regia package`'s freshness check) see a pak written by this cook.
            std::fs::File::options()
                .write(true)
                .open(&pak)
                .and_then(|f| f.set_modified(std::time::SystemTime::now()))?;
            report.kept.push(name.clone());
        } else {
            let entries: Vec<(AssetId, TypePathHash)> = ids
                .iter()
                .filter_map(|id| type_of.get(id).map(|e| (*id, TypePathHash::of(&e.type_path))))
                .collect();
            write_pak_with(
                &pak,
                &entries,
                &toc,
                &PakWriteOptions {
                    compress,
                    chunk_name: name.clone(),
                    dependencies: deps,
                    min_ratio: opts.min_ratio,
                },
                opts.gpu_meshes,
                read_cooked_from(cache),
            )?;
            write_pak_registry_sibling(cache, &pak, &toc);
            report.written.push(name.clone());
        }
        let size = std::fs::metadata(&pak)?.len();
        let hash = hash_file(&pak)?;
        inputs.chunks.insert(name.clone(), (input, hash.clone()));
        let delivery = if name == BASE_CHUNK {
            Delivery::Install
        } else {
            rule.delivery
        };
        // A transfer resumes from the last block that still matches (spec 40 § 2.6); install chunks
        // and a pak of one block never resume.
        let blocks = if delivery != Delivery::Install && size > RESUME_BLOCK {
            block_hashes(&mut std::io::BufReader::new(std::fs::File::open(&pak)?))?
        } else {
            Vec::new()
        };
        manifest.chunks.push(ChunkEntry {
            name: name.clone(),
            file,
            size,
            hash,
            delivery,
            requires: rule
                .requires
                .iter()
                .filter(|r| r.as_str() != BASE_CHUNK)
                .cloned()
                .collect(),
            overrides: rule.overrides,
            mount_order: order.get(&name).copied().unwrap_or(0),
            assets: u32::try_from(ids.len()).unwrap_or(u32::MAX),
            blocks,
        });
        report.paks.push(pak);
    }
    // Chunks the previous write made and this assignment no longer has: their files would stay in
    // `out` and the runtime mounts every pak in the folder (audit F6).
    for gone in previous.chunks.keys().filter(|n| !inputs.chunks.contains_key(*n)) {
        if !safe_chunk_name(gone) {
            continue;
        }
        let pak = out.join(format!("{gone}.fpak"));
        for file in [
            pak.clone(),
            regia_asset::registry::pak_registry_path(&pak),
            out.join(format!("{gone}{CHUNK_ENTRY_SUFFIX}")),
        ] {
            match std::fs::remove_file(&file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(target: "regia_asset_pipeline", file = %file.display(), "stale chunk file not removed: {e}")
                }
            }
        }
        report.removed.push(gone.clone());
    }
    manifest
        .write_to_dir(out)
        .map_err(|e| ChunkError::Content(e.to_string()))?;
    // Only ids a pak holds: an id the registry assigns but the cook never wrote (a failed or stale
    // cook) would read as "in a mounted chunk" and fall through to a missing cache file (audit
    // v0.2.12). The filter is the members' filter above, so `shared = "duplicate"` homes stay.
    let written =
        |id: &AssetId| type_of.contains_key(id) && cache.join(regia_asset::cooked_file_name(*id)).is_file();
    let (mapped, unwritten): (BTreeMap<AssetId, String>, BTreeMap<AssetId, String>) = a
        .chunk_of
        .iter()
        .map(|(id, c)| (*id, c.clone()))
        .partition(|(id, _)| written(id));
    if !unwritten.is_empty() {
        tracing::warn!(
            target: "regia_asset_pipeline",
            count = unwritten.len(),
            first = %unwritten.keys().next().map(ToString::to_string).unwrap_or_default(),
            "assets assigned to chunks have no cooked file; they are in no pak and no chunk"
        );
    }
    report.unwritten = unwritten.into_keys().collect();
    ChunkMap::from_assignment(&mapped)
        .write_to_dir(out)
        .map_err(|e| ChunkError::Content(e.to_string()))?;
    let text = ron::ser::to_string_pretty(&inputs, regia_core::ron_pretty())
        .map_err(|e| ChunkError::Content(e.to_string()))?;
    std::fs::write(&inputs_path, text)?;
    Ok((manifest, report))
}

/// A `[content]` size key: a positive byte count, or a size with a binary unit (`512M`, `4GiB`).
fn content_size(value: &toml::Value, key: &str) -> Result<u64, ChunkError> {
    let bytes = match value {
        toml::Value::Integer(n) => u64::try_from(*n).ok(),
        toml::Value::String(s) => crate::cook_cache::parse_size(s).ok(),
        _ => None,
    };
    bytes.filter(|b| *b > 0).ok_or_else(|| {
        ChunkError::BadRule(format!(
            "[content] {key} must be a positive size in bytes or with a unit (e.g. \"512M\")"
        ))
    })
}

/// Android store limits checked before packaging (spec 40 § 2.6). The defaults are placeholders:
/// the store changes its published limits, so the check always warns to verify them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AndroidPackLimits {
    /// Most fast-follow and on-demand packs (`[content] android_pack_limit`).
    pub max_packs: usize,
    /// Largest single fast-follow or on-demand pack in bytes (`[content] android_pack_max_size`).
    pub max_pack_bytes: u64,
    /// Largest sum of every chunk the store delivers (install, fast-follow, on-demand) in bytes
    /// (`[content] android_total_max_size`); `external` chunks do not count.
    pub max_total_bytes: u64,
}

impl Default for AndroidPackLimits {
    fn default() -> Self {
        Self {
            max_packs: DEFAULT_ANDROID_PACK_LIMIT,
            max_pack_bytes: DEFAULT_ANDROID_PACK_MAX_BYTES,
            max_total_bytes: DEFAULT_ANDROID_TOTAL_MAX_BYTES,
        }
    }
}

impl ChunkRules {
    /// The Android store limits of these rules (a size cap of 0 means its default).
    #[must_use]
    pub fn android_limits(&self) -> AndroidPackLimits {
        let or = |v: u64, d: u64| if v == 0 { d } else { v };
        AndroidPackLimits {
            max_packs: self.android_pack_limit,
            max_pack_bytes: or(self.android_pack_max_bytes, DEFAULT_ANDROID_PACK_MAX_BYTES),
            max_total_bytes: or(self.android_total_max_bytes, DEFAULT_ANDROID_TOTAL_MAX_BYTES),
        }
    }
}

/// [`check_android_packs`] plus the per-pack and total size caps of `limits` (spec 40 § 2.6). A
/// fast-follow or on-demand pack larger than `max_pack_bytes`, or a sum of every store-delivered
/// chunk (install, fast-follow, on-demand; not `external`) larger than `max_total_bytes`, fails with
/// [`ChunkError::PackSize`] (`pack_limit`) naming the chunks. Returns warnings.
///
/// # Errors
/// [`ChunkError::PackNameCollision`], [`ChunkError::PackLimit`] or [`ChunkError::PackSize`].
pub fn check_android_pack_limits(
    manifest: &ContentManifest,
    limits: &AndroidPackLimits,
) -> Result<Vec<String>, ChunkError> {
    let warnings = check_android_packs_count(manifest, limits.max_packs)?;
    let mut over: Vec<&ChunkEntry> = manifest
        .chunks
        .iter()
        .filter(|c| matches!(c.delivery, Delivery::FastFollow | Delivery::OnDemand))
        .filter(|c| c.size > limits.max_pack_bytes)
        .collect();
    over.sort_by(|a, b| a.name.cmp(&b.name));
    if let Some(first) = over.first() {
        return Err(ChunkError::PackSize {
            scope: "pack",
            bytes: first.size,
            limit: limits.max_pack_bytes,
            chunks: over.iter().map(|c| c.name.clone()).collect(),
        });
    }
    let store: Vec<&ChunkEntry> = manifest
        .chunks
        .iter()
        .filter(|c| c.delivery != Delivery::External)
        .collect();
    let total = store.iter().fold(0u64, |sum, c| sum.saturating_add(c.size));
    if total > limits.max_total_bytes {
        let mut chunks: Vec<String> = store.iter().map(|c| c.name.clone()).collect();
        chunks.sort();
        return Err(ChunkError::PackSize {
            scope: "total",
            bytes: total,
            limit: limits.max_total_bytes,
            chunks,
        });
    }
    Ok(warnings)
}

/// Android store checks before packaging (spec 40 § 2.6): pack names (chunk names with `-` → `_`)
/// must not collide, at most one fast-follow pack (the base bundle is the install-time pack), the
/// pack count stays within `limit`, and the default size caps of [`AndroidPackLimits`] hold
/// ([`check_android_pack_limits`] takes other caps). Returns warnings (count above 40).
///
/// # Errors
/// [`ChunkError::PackNameCollision`], [`ChunkError::PackLimit`] or [`ChunkError::PackSize`].
pub fn check_android_packs(manifest: &ContentManifest, limit: usize) -> Result<Vec<String>, ChunkError> {
    check_android_pack_limits(
        manifest,
        &AndroidPackLimits {
            max_packs: limit,
            ..AndroidPackLimits::default()
        },
    )
}

/// The name, fast-follow and count checks of [`check_android_packs`].
fn check_android_packs_count(manifest: &ContentManifest, limit: usize) -> Result<Vec<String>, ChunkError> {
    let packs: Vec<&ChunkEntry> = manifest
        .chunks
        .iter()
        .filter(|c| matches!(c.delivery, Delivery::FastFollow | Delivery::OnDemand))
        .collect();
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();
    for chunk in &packs {
        if let Some(other) = seen.insert(pack_name(&chunk.name), &chunk.name) {
            return Err(ChunkError::PackNameCollision(
                other.to_owned(),
                chunk.name.clone(),
            ));
        }
    }
    let fast_follow: Vec<String> = packs
        .iter()
        .filter(|c| c.delivery == Delivery::FastFollow)
        .map(|c| c.name.clone())
        .collect();
    if fast_follow.len() > 1 {
        return Err(ChunkError::PackLimit {
            count: fast_follow.len(),
            limit: 1,
            chunks: fast_follow,
        });
    }
    if packs.len() > limit {
        return Err(ChunkError::PackLimit {
            count: packs.len(),
            limit,
            chunks: packs.iter().map(|c| c.name.clone()).collect(),
        });
    }
    let mut warnings = vec![
        "Android asset-pack limits change: verify the pack count and sizes against the store's current published limits"
            .to_owned(),
    ];
    if packs.len() > ANDROID_PACK_WARN {
        warnings.push(format!(
            "{} asset packs (warning above {ANDROID_PACK_WARN})",
            packs.len()
        ));
    }
    Ok(warnings)
}

/// Cooks chunk `dlc` against a frozen base (spec 40 § 2.4): its members are the assets the rules
/// put in `dlc`, minus dependencies the base already has. A direct asset whose id is in the base
/// is [`ChunkError::DlcIdCollision`] unless the rule sets `overrides`. Writes `<dlc>.fpak`,
/// `<dlc>.chunk.ron` (its manifest entry, `requires: ["base@<base build>"]`) and
/// `<dlc>.chunk-map.bin` (its own ids, so the runtime answers `unavailable:<dlc>` for them) into
/// `out`. The runtime reads both files next to the base manifest
/// (`regia_asset::content::ContentManifest::read_from_dir_with_dlc`).
///
/// # Errors
/// Collision, unknown chunk, or a write failure.
pub fn write_dlc(
    cache: &Path,
    out: &Path,
    registry: &AssetRegistry,
    rules: &ChunkRules,
    dlc: &str,
    base: (&ContentManifest, &AssetRegistry),
    opts: &ChunkWriteOptions,
) -> Result<ChunkEntry, ChunkError> {
    let (base_manifest, base_registry) = base;
    let rule = rules
        .rules
        .get(dlc)
        .cloned()
        .ok_or_else(|| ChunkError::NotFound {
            chunk: dlc.to_owned(),
            missing: dlc.to_owned(),
        })?;
    let full = assign(registry, rules, &[])?;
    let mut members: BTreeMap<AssetId, String> = BTreeMap::new();
    for id in full.members(dlc) {
        let in_base = base_registry.by_id(id).is_some();
        let is_direct = matches!(full.reason.get(&id), Some(Reason::Rule { .. }));
        if in_base && is_direct && !rule.overrides {
            return Err(ChunkError::DlcIdCollision {
                chunk: dlc.to_owned(),
                asset: registry
                    .by_id(id)
                    .map_or_else(|| id.to_string(), |e| e.path.clone()),
            });
        }
        if in_base && !is_direct {
            continue; // resolves through the base
        }
        members.insert(id, dlc.to_owned());
    }
    let only = Assignment {
        chunk_of: members,
        ..Assignment::default()
    };
    let mut dlc_rules = ChunkRules::default();
    let mut dlc_rule = rule.clone();
    dlc_rule.requires.clear();
    dlc_rules.rules.insert(dlc.to_owned(), dlc_rule);
    // A per-call staging directory (pid + counter): two packaging jobs for one DLC must not delete
    // each other's staged files (audit concurrency-1103 pipeline L4).
    static NEXT_STAGING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT_STAGING.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let staging = out.join(format!(".{dlc}.{}-{n}.staging", std::process::id()));
    let (manifest, _) = write_chunks(cache, &staging, &only, &dlc_rules, opts)?;
    let mut entry = manifest
        .chunk(dlc)
        .cloned()
        .ok_or_else(|| ChunkError::Content(format!("chunk {dlc} was not written")))?;
    std::fs::create_dir_all(out)?;
    std::fs::rename(staging.join(&entry.file), out.join(&entry.file))?;
    let registry_name = format!("{dlc}{}", regia_asset::registry::PAK_REGISTRY_SUFFIX);
    if staging.join(&registry_name).is_file() {
        std::fs::rename(staging.join(&registry_name), out.join(&registry_name))?;
    }
    // The staged map lists only this chunk's members (audit v0.2.12: it was dropped, so a DLC id
    // had no chunk at runtime).
    let staged_map = staging.join(regia_asset::content::CHUNK_MAP_FILE);
    if staged_map.is_file() {
        std::fs::rename(
            &staged_map,
            out.join(format!("{dlc}{}", regia_asset::content::DLC_CHUNK_MAP_SUFFIX)),
        )?;
    }
    let _ = std::fs::remove_dir_all(&staging);
    entry.requires = std::iter::once(format!("{BASE_CHUNK}@{}", base_manifest.build_id))
        .chain(rule.requires.iter().filter(|r| r.as_str() != BASE_CHUNK).cloned())
        .collect();
    entry.mount_order = base_manifest
        .chunks
        .iter()
        .map(|c| c.mount_order)
        .max()
        .unwrap_or(0)
        + 1;
    let text = ron::ser::to_string_pretty(&entry, regia_core::ron_pretty())
        .map_err(|e| ChunkError::Content(e.to_string()))?;
    // The entry file is the publish step: temp + rename, so a scanner never reads it torn.
    crate::cook_ledger::write_atomic(&out.join(format!("{dlc}{CHUNK_ENTRY_SUFFIX}")), text.as_bytes())?;
    Ok(entry)
}

#[cfg(test)]
mod integrity_benchmark {
    use super::*;
    use regia_asset::registry::RegistryEntry;
    use regia_core::id::Guid;

    /// Run separately on reference hardware in release mode; timing is evidence, never a test
    /// synchronization primitive or a timing assertion. The counters are deterministic gates.
    #[test]
    #[ignore = "release benchmark on the N150 and Deck; see the CT-24 lane report"]
    fn required_edges_100k_500k() {
        const ASSETS: usize = 100_000;
        const EDGES: usize = 5;
        let id = |n: usize| AssetId(Guid::from_u128(n as u128 + 1));
        let registry = AssetRegistry::from_entries((0..ASSETS).map(|n| RegistryEntry {
            id: id(n),
            path: format!("assets/{n:06}.bin"),
            type_path: "regia_asset::CookedData".into(),
            dependencies: (1..=EDGES).map(|d| id((n + d) % ASSETS)).collect(),
            tags: Default::default(),
            size: 0,
        }));
        let shipped = registry.entries().map(|e| e.id).collect();
        let start = std::time::Instant::now();
        let report = check_dependency_integrity(&registry, &[id(0)], &shipped, &BTreeSet::new(), None);
        let micros = start.elapsed().as_micros();
        assert!(report.problems.is_empty());
        assert_eq!(report.visited_assets, ASSETS);
        assert_eq!(report.examined_edges, ASSETS * EDGES);
        assert_eq!(report.tracked_assets, ASSETS);
        assert!(report.peak_queue <= ASSETS);
        eprintln!(
            "content_dependency_integrity: {ASSETS} assets, {} edges, {micros} us, {} parent records, peak queue {}",
            ASSETS * EDGES,
            report.tracked_assets,
            report.peak_queue
        );
    }
}

#[cfg(test)]
mod startup_level_tests {
    use super::startup_level_in;

    #[test]
    fn an_empty_startup_level_names_no_root() {
        assert_eq!(startup_level_in("[project]\nstartup_level = \"\"\n"), None);
        assert_eq!(startup_level_in("[project]\nstartup_level = \"  \"\n"), None);
        assert_eq!(startup_level_in("[project]\nname = \"x\"\n"), None);
        assert_eq!(
            startup_level_in("[project]\nstartup_level = \"levels/a.level.ron\"\n").as_deref(),
            Some("levels/a.level.ron")
        );
    }
}
