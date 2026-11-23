// SPDX-License-Identifier: MPL-2.0
//! Reference graph query, reference tree, size map and audit CSV over an [`AssetSet`]
//! (spec 32 § 5, ADR-1668).
//!
//! Everything is a pure function of the set and the query: ordered maps and sorted output only, no clock,
//! no disk. Breadth-first search with a visited set walks the columns, so cycles are safe; the size
//! map's exclusive bytes are the greatest fixed point of "every referrer is a root or exclusive".
//! The treemap that draws a [`SizeMap`] (squarified layout) belongs to the editor.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::asset_graph::{AssetKind, AssetSet, DependencyGraph};
use crate::lod_classes::glob_match;

/// Most explicit roots of a [`GraphQuery`].
pub const MAX_GRAPH_ROOTS: usize = 64;
/// Most roots when the caller expanded a folder.
pub const MAX_FOLDER_ROOTS: usize = 5_000;
/// Deepest allowed hop count.
pub const MAX_GRAPH_DEPTH: u32 = 8;
/// Largest allowed column width.
pub const MAX_COLUMN_WIDTH: u32 = 500;

/// Errors of the audit queries; each has a stable lowercase code.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuditError {
    /// A root is not an asset path or id of the set.
    #[error("`{0}` is not an asset of the set")]
    UnknownAsset(String),
    /// Depth above 8 or width outside 1..=500.
    #[error("{0}")]
    BadDepth(String),
    /// More roots than allowed (or none).
    #[error("{0}")]
    TooManyRoots(String),
    /// A size-map grouping that does not parse.
    #[error("`{0}` is not a size-map grouping; use asset, kind, folder:N or tag:KEY")]
    BadGroup(String),
}

impl AuditError {
    /// Stable lowercase code for the agent protocol and the CLI.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownAsset(_) => "unknown_asset",
            Self::BadDepth(_) => "graph_bad_depth",
            Self::TooManyRoots(_) => "graph_too_many_roots",
            Self::BadGroup(_) => "size_map_bad_group",
        }
    }
}

/// Which side of the roots a column lies on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// Assets that use the roots.
    Referrers,
    /// Assets the roots use.
    Dependencies,
}

impl Side {
    /// Lowercase label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Referrers => "referrers",
            Self::Dependencies => "dependencies",
        }
    }
}

/// The kind of a reference edge; one today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum EdgeKind {
    /// A hard reference (the asset needs the target to load).
    #[serde(rename = "dep")]
    Hard,
}

impl EdgeKind {
    /// The tag printed in trees and JSON.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Hard => "dep",
        }
    }
}

/// How several roots combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Combine {
    /// Expand every root.
    #[default]
    Union,
    /// Keep only what is reachable from every root on that side.
    Intersect,
}

/// A reference graph query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphQuery {
    /// Asset paths or ids (1..=64; up to 5,000 when `folder_expanded`).
    pub roots: Vec<String>,
    /// Union or intersection of several roots.
    pub combine: Combine,
    /// Hops of referrers (0..=8).
    pub referrer_depth: u32,
    /// Hops of dependencies (0..=8).
    pub dependency_depth: u32,
    /// Most nodes listed per column (1..=500).
    pub max_per_column: u32,
    /// Only these kinds (empty: all). Roots are never filtered.
    pub kinds: Vec<AssetKind>,
    /// Only paths matching this glob.
    pub path_glob: Option<String>,
    /// Tag filters, `(key, None)` = key present, `(key, Some(v))` = key equals `v`; all must hold.
    pub tag_filters: Vec<(String, Option<String>)>,
    /// Chunk index per `(side, hop)` column, `(side, hop, page)`; default page 0.
    pub pages: Vec<(Side, u32, u32)>,
    /// The caller expanded a folder, which raises the root cap to 5,000.
    pub folder_expanded: bool,
}

impl Default for GraphQuery {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            combine: Combine::Union,
            referrer_depth: 1,
            dependency_depth: 1,
            max_per_column: 50,
            kinds: Vec::new(),
            path_glob: None,
            tag_filters: Vec::new(),
            pages: Vec::new(),
            folder_expanded: false,
        }
    }
}

/// One column of a [`GraphResult`]: the nodes first reached at one hop on one side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    /// Which side.
    pub side: Side,
    /// Hop distance from the roots (1 = direct).
    pub hop: u32,
    /// The listed nodes (one chunk of the sorted column).
    pub nodes: Vec<String>,
    /// Nodes of this column not listed (the "more" entry).
    pub hidden: usize,
    /// All nodes of the column that passed the filters.
    pub total: usize,
    /// The chunk shown (0-based).
    pub page: u32,
    /// How many chunks the column has.
    pub pages: u32,
}

/// A reference edge `from` uses `to`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Edge {
    /// The using asset.
    pub from: String,
    /// The used asset.
    pub to: String,
    /// The kind.
    pub kind: EdgeKind,
}

/// The answer to a [`GraphQuery`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphResult {
    /// The resolved root paths, sorted.
    pub roots: Vec<String>,
    /// Columns ordered referrers (far to near) are not reordered: dependencies and referrers, hop ascending.
    pub columns: Vec<Column>,
    /// Edges between the roots and listed nodes, sorted.
    pub edges: Vec<Edge>,
    /// Every kind present in the unfiltered graph, sorted.
    pub types_present: Vec<AssetKind>,
    /// Whether any column hides nodes.
    pub truncated: bool,
    /// Unresolved references of the roots and listed nodes, sorted.
    pub missing: Vec<String>,
}

/// Resolve a path or id to the set's path.
fn resolve(set: &AssetSet, name: &str) -> Option<String> {
    set.get(name)
        .or_else(|| set.assets.iter().find(|a| a.id.as_deref() == Some(name)))
        .map(|a| a.path.clone())
}

struct Filter<'a> {
    set: &'a AssetSet,
    query: &'a GraphQuery,
}

impl Filter<'_> {
    fn accepts(&self, path: &str) -> bool {
        let Some(asset) = self.set.get(path) else {
            return false;
        };
        if !self.query.kinds.is_empty() && !self.query.kinds.contains(&asset.kind) {
            return false;
        }
        if let Some(g) = &self.query.path_glob
            && !glob_match(g, path)
        {
            return false;
        }
        self.query
            .tag_filters
            .iter()
            .all(|(k, v)| match (asset.tags.get(k), v) {
                (None, _) => false,
                (Some(_), None) => true,
                (Some(have), Some(want)) => have == want,
            })
    }
}

/// Hop-by-hop expansion on one side. `allow` limits which non-root nodes may appear; `page_of`
/// picks the chunk of each column; only listed nodes are expanded further.
fn expand(
    graph: &DependencyGraph,
    roots: &[String],
    side: Side,
    depth: u32,
    width: usize,
    allow: &dyn Fn(&str) -> bool,
    page_of: &dyn Fn(u32) -> u32,
) -> (Vec<Column>, BTreeSet<Edge>) {
    let neighbors = |p: &str| match side {
        Side::Dependencies => graph.dependencies(p),
        Side::Referrers => graph.referrers(p),
    };
    let mut seen: BTreeSet<String> = roots.iter().cloned().collect();
    let mut frontier: Vec<String> = roots.to_vec();
    let mut columns = Vec::new();
    let mut edges = BTreeSet::new();
    for hop in 1..=depth {
        let mut candidates: BTreeSet<String> = BTreeSet::new();
        let mut pending: Vec<(String, String)> = Vec::new();
        for node in &frontier {
            for n in neighbors(node) {
                if roots.contains(&n) {
                    // An edge back to a root is real; the root is already listed.
                    pending.push((node.clone(), n));
                } else if allow(&n) {
                    pending.push((node.clone(), n.clone()));
                    if !seen.contains(&n) {
                        candidates.insert(n);
                    }
                }
            }
        }
        let all: Vec<String> = candidates.into_iter().collect();
        let total = all.len();
        let pages = if total == 0 {
            0
        } else {
            total.div_ceil(width) as u32
        };
        let page = page_of(hop).min(pages.saturating_sub(1));
        let start = (page as usize) * width;
        let shown: Vec<String> = all.iter().skip(start).take(width).cloned().collect();
        let shown_set: BTreeSet<&str> = shown.iter().map(String::as_str).collect();
        let hop_roots_or_seen = |p: &str| roots.iter().any(|r| r == p) || seen.contains(p);
        for (from, to) in pending {
            if shown_set.contains(to.as_str()) || hop_roots_or_seen(&to) {
                edges.insert(edge(side, &from, &to));
            }
        }
        for s in &shown {
            seen.insert(s.clone());
        }
        if total > 0 {
            columns.push(Column {
                side,
                hop,
                hidden: total - shown.len(),
                nodes: shown.clone(),
                total,
                page,
                pages,
            });
        }
        frontier = shown;
        if frontier.is_empty() {
            break;
        }
    }
    (columns, edges)
}

fn edge(side: Side, node: &str, neighbor: &str) -> Edge {
    match side {
        Side::Dependencies => Edge {
            from: node.to_owned(),
            to: neighbor.to_owned(),
            kind: EdgeKind::Hard,
        },
        Side::Referrers => Edge {
            from: neighbor.to_owned(),
            to: node.to_owned(),
            kind: EdgeKind::Hard,
        },
    }
}

/// Run a reference graph query (spec 32 § 5.1).
///
/// # Errors
///
/// [`AuditError`] for an unknown root, a depth above 8, a width outside 1..=500 or too many roots.
pub fn graph_query(
    set: &AssetSet,
    graph: &DependencyGraph,
    query: &GraphQuery,
) -> Result<GraphResult, AuditError> {
    let _span = tracing::debug_span!("asset_graph_query", roots = query.roots.len()).entered();
    let cap = if query.folder_expanded {
        MAX_FOLDER_ROOTS
    } else {
        MAX_GRAPH_ROOTS
    };
    if query.roots.is_empty() {
        return Err(AuditError::TooManyRoots("at least one root is required".into()));
    }
    if query.roots.len() > cap {
        return Err(AuditError::TooManyRoots(format!(
            "{} roots given; the limit is {cap}",
            query.roots.len()
        )));
    }
    if query.referrer_depth > MAX_GRAPH_DEPTH || query.dependency_depth > MAX_GRAPH_DEPTH {
        return Err(AuditError::BadDepth(format!(
            "depth is at most {MAX_GRAPH_DEPTH}"
        )));
    }
    if query.max_per_column == 0 || query.max_per_column > MAX_COLUMN_WIDTH {
        return Err(AuditError::BadDepth(format!(
            "max_per_column must be 1..={MAX_COLUMN_WIDTH}"
        )));
    }
    let mut roots: Vec<String> = Vec::new();
    for r in &query.roots {
        let path = resolve(set, r).ok_or_else(|| AuditError::UnknownAsset(r.clone()))?;
        if !roots.contains(&path) {
            roots.push(path);
        }
    }
    roots.sort();
    let filter = Filter { set, query };
    let width = query.max_per_column as usize;
    let mut columns = Vec::new();
    let mut edges: BTreeSet<Edge> = BTreeSet::new();
    for (side, depth) in [
        (Side::Dependencies, query.dependency_depth),
        (Side::Referrers, query.referrer_depth),
    ] {
        let page_of = |hop: u32| {
            query
                .pages
                .iter()
                .find(|(s, h, _)| *s == side && *h == hop)
                .map_or(0, |(_, _, p)| *p)
        };
        let intersect = query.combine == Combine::Intersect && roots.len() > 1;
        let common: Option<BTreeSet<String>> = if intersect {
            // Per root, everything it reaches on this side; keep what all roots reach.
            let mut sets = roots.iter().map(|r| {
                let (cols, _) = expand(
                    graph,
                    std::slice::from_ref(r),
                    side,
                    depth,
                    usize::MAX / 2,
                    &|p| filter.accepts(p),
                    &|_| 0,
                );
                cols.into_iter()
                    .flat_map(|c| c.nodes)
                    .collect::<BTreeSet<String>>()
            });
            let first = sets.next().unwrap_or_default();
            Some(sets.fold(first, |acc, s| acc.intersection(&s).cloned().collect()))
        } else {
            None
        };
        let allow = |p: &str| filter.accepts(p) && common.as_ref().is_none_or(|c| c.contains(p));
        let (cols, es) = expand(graph, &roots, side, depth, width, &allow, &page_of);
        columns.extend(cols);
        edges.extend(es);
    }
    let listed: BTreeSet<&str> = roots
        .iter()
        .map(String::as_str)
        .chain(columns.iter().flat_map(|c| c.nodes.iter().map(String::as_str)))
        .collect();
    let edges: Vec<Edge> = edges
        .into_iter()
        .filter(|e| listed.contains(e.from.as_str()) && listed.contains(e.to.as_str()))
        .collect();
    let mut missing: BTreeSet<String> = BTreeSet::new();
    for p in &listed {
        missing.extend(graph.missing_references(p));
    }
    let mut types_present: Vec<AssetKind> = set.assets.iter().map(|a| a.kind).collect();
    types_present.sort();
    types_present.dedup();
    let truncated = columns.iter().any(|c| c.hidden > 0);
    Ok(GraphResult {
        roots,
        columns,
        edges,
        types_present,
        truncated,
        missing: missing.into_iter().collect(),
    })
}

/// The reference tree text of one asset (spec 32 § 5.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeResult {
    /// One asset per line, indented two spaces per level, `dep <path>` below the root.
    pub text: String,
    /// Lines printed below the root.
    pub nodes: usize,
    /// Whether `max_nodes` cut the output.
    pub truncated: bool,
}

/// Print the indented reference tree of `root` on `side`. A node already on the current chain is
/// printed with a `(cycle)` suffix and not expanded; output ends with `(truncated)` at `max_nodes`.
///
/// # Errors
///
/// [`AuditError::UnknownAsset`] when `root` is not in the set, [`AuditError::BadDepth`] for a depth over 64.
pub fn reference_tree(
    set: &AssetSet,
    graph: &DependencyGraph,
    root: &str,
    side: Side,
    max_depth: u32,
    max_nodes: usize,
    include_sub_assets: bool,
) -> Result<TreeResult, AuditError> {
    let root = resolve(set, root).ok_or_else(|| AuditError::UnknownAsset(root.to_owned()))?;
    if max_depth > 64 {
        return Err(AuditError::BadDepth("max_depth is at most 64".into()));
    }
    struct Walk<'a> {
        graph: &'a DependencyGraph,
        side: Side,
        max_depth: u32,
        max_nodes: usize,
        sub: bool,
        out: String,
        nodes: usize,
        truncated: bool,
        chain: Vec<String>,
    }
    impl Walk<'_> {
        fn visit(&mut self, node: &str, depth: u32) {
            if depth >= self.max_depth {
                return;
            }
            let children = match self.side {
                Side::Dependencies => self.graph.dependencies(node),
                Side::Referrers => self.graph.referrers(node),
            };
            for child in children {
                if !self.sub && child.contains('#') {
                    continue;
                }
                if self.nodes >= self.max_nodes {
                    self.truncated = true;
                    return;
                }
                self.nodes += 1;
                let indent = "  ".repeat(depth as usize + 1);
                let cycle = self.chain.contains(&child);
                self.out.push_str(&format!(
                    "{indent}{} {child}{}\n",
                    EdgeKind::Hard.tag(),
                    if cycle { " (cycle)" } else { "" }
                ));
                if !cycle {
                    self.chain.push(child.clone());
                    self.visit(&child, depth + 1);
                    self.chain.pop();
                    if self.truncated {
                        return;
                    }
                }
            }
        }
    }
    let mut walk = Walk {
        graph,
        side,
        max_depth,
        max_nodes,
        sub: include_sub_assets,
        out: format!("{root}\n"),
        nodes: 0,
        truncated: false,
        chain: vec![root.clone()],
    };
    walk.visit(&root, 0);
    if walk.truncated {
        walk.out.push_str("(truncated)\n");
    }
    Ok(TreeResult {
        text: walk.out,
        nodes: walk.nodes,
        truncated: walk.truncated,
    })
}

/// How a [`SizeMap`] groups the closure.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    /// One row per asset, the smallest folded into `(other)` past `top`.
    #[default]
    Asset,
    /// One row per asset kind.
    Kind,
    /// One row per folder of the first `n` path segments.
    Folder(u32),
    /// One row per value of a tag key (`(none)` when absent).
    Tag(String),
}

impl GroupBy {
    /// Parse `asset`, `kind`, `folder:N` or `tag:KEY`.
    ///
    /// # Errors
    ///
    /// [`AuditError::BadGroup`] for anything else.
    pub fn parse(text: &str) -> Result<Self, AuditError> {
        let bad = || AuditError::BadGroup(text.to_owned());
        match text {
            "asset" => Ok(Self::Asset),
            "kind" => Ok(Self::Kind),
            _ => {
                if let Some(n) = text.strip_prefix("folder:") {
                    let n: u32 = n.parse().map_err(|_| bad())?;
                    if n == 0 {
                        return Err(bad());
                    }
                    Ok(Self::Folder(n))
                } else if let Some(k) = text.strip_prefix("tag:") {
                    if k.is_empty() {
                        return Err(bad());
                    }
                    Ok(Self::Tag(k.to_owned()))
                } else {
                    Err(bad())
                }
            }
        }
    }
}

/// A size map query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SizeMapQuery {
    /// Asset paths or ids.
    pub roots: Vec<String>,
    /// How rows group.
    pub group_by: GroupBy,
    /// Rows kept in `Asset` grouping before folding into `(other)`.
    pub top: u32,
}

impl Default for SizeMapQuery {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            group_by: GroupBy::Asset,
            top: 200,
        }
    }
}

/// One row of a [`SizeMap`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeRow {
    /// Asset path, kind label, folder prefix or tag value.
    pub name: String,
    /// Assets in the row.
    pub assets: u64,
    /// Cooked bytes of the row's assets.
    pub bytes: u64,
    /// Bytes that would become unreferenced if all roots were removed.
    pub exclusive_bytes: u64,
    /// Bytes of non-root members something outside the roots' exclusive set still uses.
    pub shared_bytes: u64,
}

/// The footprint of a closure (spec 32 § 5.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeMap {
    /// Assets in the closure (roots included).
    pub closure_assets: u64,
    /// Cooked bytes of the closure, each asset once.
    pub total_bytes: u64,
    /// Bytes of the roots themselves.
    pub roots_bytes: u64,
    /// Bytes of non-root members that are not exclusive.
    pub shared_bytes: u64,
    /// Bytes of non-root members that are exclusive.
    pub exclusive_bytes: u64,
    /// The rows, bytes descending then name.
    pub rows: Vec<SizeRow>,
    /// Unresolved dependency ids (0 bytes), sorted.
    pub missing: Vec<String>,
}

struct Closure {
    members: BTreeSet<String>,
    exclusive: BTreeSet<String>,
    roots: BTreeSet<String>,
}

fn closure(graph: &DependencyGraph, roots: &[String]) -> Closure {
    let members = graph.reachable_from(roots.iter().map(String::as_str));
    let root_set: BTreeSet<String> = roots.iter().cloned().collect();
    // Greatest fixed point: start with every non-root member, drop those with an outside referrer.
    let mut exclusive: BTreeSet<String> = members.difference(&root_set).cloned().collect();
    loop {
        let drop: Vec<String> = exclusive
            .iter()
            .filter(|m| {
                graph
                    .referrers(m)
                    .iter()
                    .any(|r| !root_set.contains(r) && !exclusive.contains(r))
            })
            .cloned()
            .collect();
        if drop.is_empty() {
            break;
        }
        for d in drop {
            exclusive.remove(&d);
        }
    }
    Closure {
        members,
        exclusive,
        roots: root_set,
    }
}

/// Group name of `path` under `by`.
fn group_name(set: &AssetSet, path: &str, by: &GroupBy) -> String {
    let Some(asset) = set.get(path) else {
        return path.to_owned();
    };
    match by {
        GroupBy::Asset => path.to_owned(),
        GroupBy::Kind => asset.kind.label().to_owned(),
        GroupBy::Folder(n) => {
            let dirs: Vec<&str> = path.split('/').collect();
            let dirs = &dirs[..dirs.len() - 1];
            if dirs.is_empty() {
                "(root)".to_owned()
            } else {
                dirs[..dirs.len().min(*n as usize)].join("/")
            }
        }
        GroupBy::Tag(k) => asset.tags.get(k).cloned().unwrap_or_else(|| "(none)".to_owned()),
    }
}

/// Build the size map of the closure of `query.roots`.
///
/// # Errors
///
/// [`AuditError::UnknownAsset`] for a root that is not in the set, [`AuditError::TooManyRoots`] for none.
pub fn size_map(
    set: &AssetSet,
    graph: &DependencyGraph,
    query: &SizeMapQuery,
) -> Result<SizeMap, AuditError> {
    let _span = tracing::debug_span!("asset_size_map", roots = query.roots.len()).entered();
    if query.roots.is_empty() {
        return Err(AuditError::TooManyRoots("at least one root is required".into()));
    }
    let mut roots = Vec::new();
    for r in &query.roots {
        let p = resolve(set, r).ok_or_else(|| AuditError::UnknownAsset(r.clone()))?;
        if !roots.contains(&p) {
            roots.push(p);
        }
    }
    let c = closure(graph, &roots);
    let bytes_of = |p: &str| graph.own_bytes(p).unwrap_or(0);
    let mut rows: BTreeMap<String, SizeRow> = BTreeMap::new();
    let mut total = 0u64;
    let mut roots_bytes = 0u64;
    let mut shared = 0u64;
    let mut exclusive_total = 0u64;
    let mut missing: BTreeSet<String> = BTreeSet::new();
    for m in &c.members {
        let b = bytes_of(m);
        total = total.saturating_add(b);
        missing.extend(graph.missing_references(m));
        let row = rows
            .entry(group_name(set, m, &query.group_by))
            .or_insert_with_key(|k| SizeRow {
                name: k.clone(),
                assets: 0,
                bytes: 0,
                exclusive_bytes: 0,
                shared_bytes: 0,
            });
        row.assets += 1;
        row.bytes = row.bytes.saturating_add(b);
        if c.roots.contains(m) {
            roots_bytes = roots_bytes.saturating_add(b);
        } else if c.exclusive.contains(m) {
            row.exclusive_bytes = row.exclusive_bytes.saturating_add(b);
            exclusive_total = exclusive_total.saturating_add(b);
        } else {
            row.shared_bytes = row.shared_bytes.saturating_add(b);
            shared = shared.saturating_add(b);
        }
    }
    let mut rows: Vec<SizeRow> = rows.into_values().collect();
    rows.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    if query.group_by == GroupBy::Asset && rows.len() > query.top as usize {
        let rest = rows.split_off(query.top as usize);
        let mut other = SizeRow {
            name: "(other)".to_owned(),
            assets: 0,
            bytes: 0,
            exclusive_bytes: 0,
            shared_bytes: 0,
        };
        for r in rest {
            other.assets += r.assets;
            other.bytes = other.bytes.saturating_add(r.bytes);
            other.exclusive_bytes = other.exclusive_bytes.saturating_add(r.exclusive_bytes);
            other.shared_bytes = other.shared_bytes.saturating_add(r.shared_bytes);
        }
        rows.push(other);
    }
    Ok(SizeMap {
        closure_assets: c.members.len() as u64,
        total_bytes: total,
        roots_bytes,
        shared_bytes: shared,
        exclusive_bytes: exclusive_total,
        rows,
        missing: missing.into_iter().collect(),
    })
}

/// The header of [`audit_csv`].
pub const AUDIT_CSV_HEADER: [&str; 9] = [
    "path",
    "kind",
    "size",
    "dependency_count",
    "referrer_count",
    "closure_assets",
    "closure_bytes",
    "exclusive_bytes",
    "tags",
];

/// The audit table (RFC 4180) for `selection` (empty: every asset), rows sorted by path. The closure
/// and exclusive columns use the size-map definitions with the row's asset as the only root.
#[must_use]
pub fn audit_csv(set: &AssetSet, graph: &DependencyGraph, selection: &[String]) -> String {
    let mut paths: Vec<String> = if selection.is_empty() {
        set.assets.iter().map(|a| a.path.clone()).collect()
    } else {
        selection.iter().filter_map(|s| resolve(set, s)).collect()
    };
    paths.sort();
    paths.dedup();
    let mut records: Vec<Vec<String>> = vec![AUDIT_CSV_HEADER.iter().map(|s| (*s).to_owned()).collect()];
    for path in &paths {
        let Some(asset) = set.get(path) else { continue };
        let c = closure(graph, std::slice::from_ref(path));
        let closure_bytes = c
            .members
            .iter()
            .fold(0u64, |acc, m| acc.saturating_add(graph.own_bytes(m).unwrap_or(0)));
        let exclusive_bytes = c
            .exclusive
            .iter()
            .fold(0u64, |acc, m| acc.saturating_add(graph.own_bytes(m).unwrap_or(0)));
        let tags = asset
            .tags
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(";");
        records.push(vec![
            path.clone(),
            asset.kind.label().to_owned(),
            asset.cooked_bytes.to_string(),
            graph.dependencies(path).len().to_string(),
            graph.referrers(path).len().to_string(),
            c.members.len().to_string(),
            closure_bytes.to_string(),
            exclusive_bytes.to_string(),
            tags,
        ]);
    }
    regia_asset::csv::to_string(&records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_graph::AssetEntry;

    fn build(set: &AssetSet) -> DependencyGraph {
        DependencyGraph::build(set)
    }

    fn chain_set() -> AssetSet {
        // a -> b -> c -> d, e -> c
        let mut s = AssetSet::new();
        s.push(
            AssetEntry::new("a", AssetKind::Scene)
                .with_references(["b"])
                .with_cooked_bytes(1),
        );
        s.push(
            AssetEntry::new("b", AssetKind::Mesh)
                .with_references(["c"])
                .with_cooked_bytes(10),
        );
        s.push(
            AssetEntry::new("c", AssetKind::Material)
                .with_references(["d"])
                .with_cooked_bytes(100),
        );
        s.push(AssetEntry::new("d", AssetKind::Texture).with_cooked_bytes(1000));
        s.push(
            AssetEntry::new("e", AssetKind::Scene)
                .with_references(["c"])
                .with_cooked_bytes(2),
        );
        s
    }

    fn q(roots: &[&str]) -> GraphQuery {
        GraphQuery {
            roots: roots.iter().map(|s| (*s).to_owned()).collect(),
            ..GraphQuery::default()
        }
    }

    fn col(r: &GraphResult, side: Side, hop: u32) -> Option<&Column> {
        r.columns.iter().find(|c| c.side == side && c.hop == hop)
    }

    #[test]
    fn depth_two_shows_related_of_related() {
        let s = chain_set();
        let g = build(&s);
        let mut query = q(&["b"]);
        query.dependency_depth = 2;
        query.referrer_depth = 2;
        let r = graph_query(&s, &g, &query).expect("query");
        assert_eq!(
            col(&r, Side::Dependencies, 1).map(|c| c.nodes.clone()),
            Some(vec!["c".into()])
        );
        assert_eq!(
            col(&r, Side::Dependencies, 2).map(|c| c.nodes.clone()),
            Some(vec!["d".into()])
        );
        assert_eq!(
            col(&r, Side::Referrers, 1).map(|c| c.nodes.clone()),
            Some(vec!["a".into()])
        );
        assert!(col(&r, Side::Referrers, 2).is_none());
        assert_eq!(r.edges.len(), 3);
        let depth1 = graph_query(&s, &g, &q(&["b"])).expect("query");
        assert!(col(&depth1, Side::Dependencies, 2).is_none());
    }

    #[test]
    fn width_limit_hidden_and_pages() {
        let mut s = AssetSet::new();
        let refs: Vec<String> = (0..20).map(|i| format!("t{i:02}")).collect();
        s.push(AssetEntry::new("m", AssetKind::Mesh).with_references(refs.clone()));
        for r in &refs {
            s.push(AssetEntry::new(r.clone(), AssetKind::Texture));
        }
        let g = build(&s);
        let mut query = q(&["m"]);
        query.max_per_column = 10;
        let r = graph_query(&s, &g, &query).expect("query");
        let c = col(&r, Side::Dependencies, 1).expect("column");
        assert_eq!((c.nodes.len(), c.hidden, c.total, c.pages), (10, 10, 20, 2));
        assert_eq!(c.nodes[0], "t00");
        assert!(r.truncated);
        assert_eq!(r.edges.len(), 10);
        query.pages = vec![(Side::Dependencies, 1, 1)];
        let r = graph_query(&s, &g, &query).expect("query");
        let c = col(&r, Side::Dependencies, 1).expect("column");
        assert_eq!(
            (c.nodes[0].as_str(), c.nodes.len(), c.hidden, c.page),
            ("t10", 10, 10, 1)
        );
        query.max_per_column = 20;
        let r = graph_query(&s, &g, &query).expect("query");
        assert!(!r.truncated);
    }

    #[test]
    fn intersect_keeps_shared_nodes_only() {
        let mut s = AssetSet::new();
        s.push(AssetEntry::new("x", AssetKind::Mesh).with_references(["shared", "only_x"]));
        s.push(AssetEntry::new("y", AssetKind::Mesh).with_references(["shared", "only_y"]));
        for p in ["shared", "only_x", "only_y"] {
            s.push(AssetEntry::new(p, AssetKind::Texture));
        }
        let g = build(&s);
        let mut query = q(&["x", "y"]);
        query.combine = Combine::Intersect;
        let r = graph_query(&s, &g, &query).expect("query");
        assert_eq!(
            col(&r, Side::Dependencies, 1).map(|c| c.nodes.clone()),
            Some(vec!["shared".into()])
        );
        query.combine = Combine::Union;
        let r = graph_query(&s, &g, &query).expect("query");
        assert_eq!(col(&r, Side::Dependencies, 1).map(|c| c.nodes.len()), Some(3));
        // none in common
        let mut s2 = AssetSet::new();
        s2.push(AssetEntry::new("p", AssetKind::Mesh).with_references(["pp"]));
        s2.push(AssetEntry::new("q", AssetKind::Mesh).with_references(["qq"]));
        s2.push(AssetEntry::new("pp", AssetKind::Texture));
        s2.push(AssetEntry::new("qq", AssetKind::Texture));
        let g2 = build(&s2);
        let mut query = q(&["p", "q"]);
        query.combine = Combine::Intersect;
        let r = graph_query(&s2, &g2, &query).expect("query");
        assert!(r.columns.is_empty());
    }

    #[test]
    fn unreferenced_asset_has_empty_referrer_side_and_cycle_appears_once() {
        let mut s = AssetSet::new();
        s.push(AssetEntry::new("a", AssetKind::Other).with_references(["b"]));
        s.push(AssetEntry::new("b", AssetKind::Other).with_references(["c"]));
        s.push(AssetEntry::new("c", AssetKind::Other).with_references(["a"]));
        s.push(AssetEntry::new("lonely", AssetKind::Other));
        let g = build(&s);
        let r = graph_query(&s, &g, &q(&["lonely"])).expect("query");
        assert!(r.columns.is_empty() && r.edges.is_empty());
        let mut query = q(&["a"]);
        query.dependency_depth = 8;
        query.referrer_depth = 0;
        let r = graph_query(&s, &g, &query).expect("query");
        let listed: Vec<&String> = r.columns.iter().flat_map(|c| &c.nodes).collect();
        assert_eq!(listed, ["b", "c"]);
        assert!(r.edges.contains(&Edge {
            from: "c".into(),
            to: "a".into(),
            kind: EdgeKind::Hard
        }));
    }

    #[test]
    fn filters_hide_nodes_but_not_types_present_or_roots() {
        let mut s = AssetSet::new();
        s.push(
            AssetEntry::new("m", AssetKind::Mesh)
                .with_references(["t.png", "mat", "a.wav"])
                .with_tag("team", "art"),
        );
        s.push(AssetEntry::new("t.png", AssetKind::Texture).with_tag("team", "art"));
        s.push(AssetEntry::new("mat", AssetKind::Material));
        s.push(AssetEntry::new("a.wav", AssetKind::Audio).with_tag("team", "sfx"));
        let g = build(&s);
        let mut query = q(&["m"]);
        query.kinds = vec![AssetKind::Texture];
        let r = graph_query(&s, &g, &query).expect("query");
        assert_eq!(
            col(&r, Side::Dependencies, 1).map(|c| c.nodes.clone()),
            Some(vec!["t.png".into()])
        );
        assert_eq!(r.types_present.len(), 4);
        assert_eq!(r.roots, ["m"]);
        let mut query = q(&["m"]);
        query.tag_filters = vec![("team".into(), Some("sfx".into()))];
        let r = graph_query(&s, &g, &query).expect("query");
        assert_eq!(
            col(&r, Side::Dependencies, 1).map(|c| c.nodes.clone()),
            Some(vec!["a.wav".into()])
        );
        let mut query = q(&["m"]);
        query.path_glob = Some("**.png".into());
        let r = graph_query(&s, &g, &query).expect("query");
        assert_eq!(
            col(&r, Side::Dependencies, 1).map(|c| c.nodes.clone()),
            Some(vec!["t.png".into()])
        );
        let mut query = q(&["m"]);
        query.tag_filters = vec![("team".into(), None)];
        let r = graph_query(&s, &g, &query).expect("query");
        assert_eq!(col(&r, Side::Dependencies, 1).map(|c| c.nodes.len()), Some(2));
    }

    #[test]
    fn query_errors_have_codes() {
        let s = chain_set();
        let g = build(&s);
        let mut query = q(&["a"]);
        query.dependency_depth = 9;
        assert_eq!(
            graph_query(&s, &g, &query).expect_err("depth").code(),
            "graph_bad_depth"
        );
        let mut query = q(&["a"]);
        query.max_per_column = 501;
        assert_eq!(
            graph_query(&s, &g, &query).expect_err("width").code(),
            "graph_bad_depth"
        );
        query.max_per_column = 0;
        assert_eq!(
            graph_query(&s, &g, &query).expect_err("width").code(),
            "graph_bad_depth"
        );
        assert_eq!(
            graph_query(&s, &g, &q(&["nope"])).expect_err("unknown").code(),
            "unknown_asset"
        );
        let many = GraphQuery {
            roots: (0..65).map(|i| format!("r{i}")).collect(),
            ..GraphQuery::default()
        };
        assert_eq!(
            graph_query(&s, &g, &many).expect_err("roots").code(),
            "graph_too_many_roots"
        );
        assert_eq!(
            graph_query(&s, &g, &q(&[])).expect_err("none").code(),
            "graph_too_many_roots"
        );
    }

    #[test]
    fn ids_resolve_to_paths() {
        let mut s = chain_set();
        s.assets[1].id = Some("id-b".into());
        let g = build(&s);
        let r = graph_query(&s, &g, &q(&["id-b"])).expect("query");
        assert_eq!(r.roots, ["b"]);
    }

    #[test]
    fn tree_indents_marks_cycles_and_truncates() {
        let mut s = AssetSet::new();
        s.push(AssetEntry::new("a", AssetKind::Other).with_references(["b", "c"]));
        s.push(AssetEntry::new("b", AssetKind::Other).with_references(["a"]));
        s.push(AssetEntry::new("c#sub", AssetKind::Other));
        s.push(AssetEntry::new("c", AssetKind::Other).with_references(["c#sub"]));
        let g = build(&s);
        let t = reference_tree(&s, &g, "a", Side::Dependencies, 8, 100, true).expect("tree");
        assert_eq!(t.text, "a\n  dep b\n    dep a (cycle)\n  dep c\n    dep c#sub\n");
        assert_eq!(t.nodes, 4);
        let t = reference_tree(&s, &g, "a", Side::Dependencies, 8, 100, false).expect("tree");
        assert!(!t.text.contains("c#sub"));
        let t = reference_tree(&s, &g, "a", Side::Dependencies, 8, 2, true).expect("tree");
        assert!(t.truncated);
        assert!(t.text.ends_with("(truncated)\n"));
        assert_eq!(t.nodes, 2);
        let t = reference_tree(&s, &g, "b", Side::Referrers, 1, 100, true).expect("tree");
        assert_eq!(t.text, "b\n  dep a\n");
        assert_eq!(
            reference_tree(&s, &g, "zz", Side::Dependencies, 8, 10, true)
                .expect_err("unknown")
                .code(),
            "unknown_asset"
        );
    }

    #[test]
    fn size_map_diamond_counts_the_leaf_once() {
        // root -> l, r; l -> leaf; r -> leaf
        let mut s = AssetSet::new();
        s.push(
            AssetEntry::new("root", AssetKind::Scene)
                .with_references(["l", "r"])
                .with_cooked_bytes(1),
        );
        s.push(
            AssetEntry::new("l", AssetKind::Mesh)
                .with_references(["leaf"])
                .with_cooked_bytes(10),
        );
        s.push(
            AssetEntry::new("r", AssetKind::Mesh)
                .with_references(["leaf"])
                .with_cooked_bytes(20),
        );
        s.push(AssetEntry::new("leaf", AssetKind::Texture).with_cooked_bytes(100));
        let g = build(&s);
        let m = size_map(
            &s,
            &g,
            &SizeMapQuery {
                roots: vec!["root".into()],
                ..SizeMapQuery::default()
            },
        )
        .expect("map");
        assert_eq!(m.total_bytes, 131);
        assert_eq!(m.closure_assets, 4);
        assert_eq!(m.roots_bytes, 1);
        assert_eq!(m.exclusive_bytes, 130);
        assert_eq!(m.shared_bytes, 0);
        assert_eq!(m.rows.iter().map(|r| r.bytes).sum::<u64>(), m.total_bytes);
    }

    #[test]
    fn size_map_external_referrer_makes_members_shared_and_chains_follow() {
        // root -> a -> b -> c ; outsider -> b
        let mut s = AssetSet::new();
        s.push(
            AssetEntry::new("root", AssetKind::Scene)
                .with_references(["a"])
                .with_cooked_bytes(1),
        );
        s.push(
            AssetEntry::new("a", AssetKind::Mesh)
                .with_references(["b"])
                .with_cooked_bytes(10),
        );
        s.push(
            AssetEntry::new("b", AssetKind::Mesh)
                .with_references(["c"])
                .with_cooked_bytes(100),
        );
        s.push(AssetEntry::new("c", AssetKind::Texture).with_cooked_bytes(1000));
        s.push(
            AssetEntry::new("outsider", AssetKind::Scene)
                .with_references(["b"])
                .with_cooked_bytes(5),
        );
        let g = build(&s);
        let m = size_map(
            &s,
            &g,
            &SizeMapQuery {
                roots: vec!["root".into()],
                ..SizeMapQuery::default()
            },
        )
        .expect("map");
        // a is exclusive; b has an outside referrer so it and c (only reached through b) are shared.
        assert_eq!(m.exclusive_bytes, 10);
        assert_eq!(m.shared_bytes, 1100);
        let row = |n: &str| m.rows.iter().find(|r| r.name == n).cloned().expect("row");
        assert_eq!(row("a").exclusive_bytes, 10);
        assert_eq!(row("b").shared_bytes, 100);
        assert_eq!(row("c").shared_bytes, 1000);
    }

    #[test]
    fn size_map_cycle_members_are_exclusive_and_missing_listed() {
        let mut s = AssetSet::new();
        s.push(
            AssetEntry::new("root", AssetKind::Scene)
                .with_references(["a", "unresolved:ffff"])
                .with_cooked_bytes(1),
        );
        s.push(
            AssetEntry::new("a", AssetKind::Mesh)
                .with_references(["b"])
                .with_cooked_bytes(10),
        );
        s.push(
            AssetEntry::new("b", AssetKind::Mesh)
                .with_references(["a"])
                .with_cooked_bytes(20),
        );
        let g = build(&s);
        let m = size_map(
            &s,
            &g,
            &SizeMapQuery {
                roots: vec!["root".into()],
                ..SizeMapQuery::default()
            },
        )
        .expect("map");
        assert_eq!(m.exclusive_bytes, 30);
        assert_eq!(m.missing, ["unresolved:ffff"]);
    }

    #[test]
    fn size_map_groups_sum_to_total_including_other() {
        let mut s = AssetSet::new();
        s.push(
            AssetEntry::new("lvl/root", AssetKind::Scene)
                .with_references(["tex/a", "tex/b", "mesh/c"])
                .with_cooked_bytes(1),
        );
        s.push(
            AssetEntry::new("tex/a", AssetKind::Texture)
                .with_cooked_bytes(50)
                .with_tag("team", "art"),
        );
        s.push(AssetEntry::new("tex/b", AssetKind::Texture).with_cooked_bytes(40));
        s.push(AssetEntry::new("mesh/c", AssetKind::Mesh).with_cooked_bytes(30));
        let g = build(&s);
        let roots = vec!["lvl/root".to_owned()];
        for (by, expect_rows) in [
            (GroupBy::Asset, 4),
            (GroupBy::Kind, 3),
            (GroupBy::Folder(1), 3),
            (GroupBy::Tag("team".into()), 2),
        ] {
            let m = size_map(
                &s,
                &g,
                &SizeMapQuery {
                    roots: roots.clone(),
                    group_by: by.clone(),
                    top: 200,
                },
            )
            .expect("map");
            assert_eq!(m.rows.len(), expect_rows, "{by:?}");
            assert_eq!(
                m.rows.iter().map(|r| r.bytes).sum::<u64>(),
                m.total_bytes,
                "{by:?}"
            );
        }
        let m = size_map(
            &s,
            &g,
            &SizeMapQuery {
                roots,
                group_by: GroupBy::Asset,
                top: 2,
            },
        )
        .expect("map");
        assert_eq!(m.rows.len(), 3);
        assert_eq!(m.rows[2].name, "(other)");
        assert_eq!(m.rows[2].assets, 2);
        assert_eq!(m.rows.iter().map(|r| r.bytes).sum::<u64>(), 121);
        assert_eq!(m.rows[0].name, "tex/a");
    }

    #[test]
    fn group_by_parse() {
        assert_eq!(GroupBy::parse("folder:2").expect("parse"), GroupBy::Folder(2));
        assert_eq!(
            GroupBy::parse("tag:team").expect("parse"),
            GroupBy::Tag("team".into())
        );
        for bad in ["folder:0", "folder:x", "tag:", "nope"] {
            assert_eq!(
                GroupBy::parse(bad).expect_err("bad").code(),
                "size_map_bad_group",
                "{bad}"
            );
        }
    }

    #[test]
    fn audit_csv_header_order_quoting_and_sorting() {
        let mut s = AssetSet::new();
        s.push(AssetEntry::new("z.png", AssetKind::Texture).with_cooked_bytes(5));
        s.push(
            AssetEntry::new("dir/a,\"b\".mesh", AssetKind::Mesh)
                .with_references(["z.png"])
                .with_cooked_bytes(7)
                .with_tag("team", "art")
                .with_tag("lod", "0"),
        );
        let g = build(&s);
        let csv = audit_csv(&s, &g, &[]);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(
            lines[0],
            "path,kind,size,dependency_count,referrer_count,closure_assets,closure_bytes,exclusive_bytes,tags"
        );
        assert_eq!(
            lines[1],
            "\"dir/a,\"\"b\"\".mesh\",mesh,7,1,0,2,12,5,lod=0;team=art"
        );
        assert_eq!(lines[2], "z.png,texture,5,0,1,1,5,0,");
        let one = audit_csv(&s, &g, &["z.png".into()]);
        assert_eq!(one.lines().count(), 2);
        assert_eq!(audit_csv(&s, &g, &[]), csv);
    }
}
