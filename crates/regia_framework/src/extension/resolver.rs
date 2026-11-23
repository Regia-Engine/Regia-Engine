// SPDX-License-Identifier: MPL-2.0
//! Extension resolution (`specs/57` § 2.5): a pure function from discovered manifests and the
//! project's enable lists to an ordered set and diagnostics.

use std::collections::{BTreeMap, BTreeSet};

use semver::Version;

use super::manifest::{Dependency, ExtensionManifest, IMPLICIT_CAPABILITIES, KNOWN_CAPABILITIES, Phase};

/// How serious a diagnostic is. Only errors fail an extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Never blocks.
    Info,
    /// Never blocks.
    Warning,
    /// Fails the extension and every root that needs it.
    Error,
}

/// One finding: a lowercase code (spec § 5.3), the extension it concerns and a message.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Diagnostic {
    /// The extension id.
    pub id: String,
    /// Lowercase code.
    pub code: &'static str,
    /// Severity.
    pub severity: Severity,
    /// Human-readable detail.
    pub message: String,
}

fn diag(id: &str, code: &'static str, severity: Severity, message: impl Into<String>) -> Diagnostic {
    Diagnostic {
        id: id.to_owned(),
        code,
        severity,
        message: message.into(),
    }
}

/// A manifest found on disk, with where and its content hash.
#[derive(Debug, Clone)]
pub struct DiscoveredManifest {
    /// The parsed manifest.
    pub manifest: ExtensionManifest,
    /// Where it was found (compared to tell a duplicate from a re-scan).
    pub path: String,
    /// Hash of the manifest bytes (hex), part of the resolved hash.
    pub hash: String,
}

/// Everything the resolver reads.
#[derive(Debug, Clone)]
pub struct ResolveInput<'a> {
    /// Manifests in discovery order (the first of a duplicate id wins).
    pub discovered: &'a [DiscoveredManifest],
    /// The project's `enabled` roots.
    pub enabled: &'a [String],
    /// The project's `disabled` list; beats `auto_enable` and dependencies.
    pub disabled: &'a [String],
    /// `[extensions.grants.<id>]`.
    pub grants: &'a BTreeMap<String, Vec<String>>,
    /// The running engine version.
    pub engine_version: &'a Version,
    /// `linux`, `windows`, `android` or `macos`.
    pub platform: &'a str,
    /// `dev` or `shipping`.
    pub profile: &'a str,
    /// Whether this platform can load code at run time (`platform.dynamic_code`).
    pub dynamic_code: bool,
    /// Engine agent namespaces an id may not equal.
    pub reserved_ids: &'a [String],
}

/// Where an extension ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExtensionState {
    /// In the resolved order.
    Active,
    /// A resolution error; reasons are in the diagnostics.
    Failed,
    /// Removed by a platform or profile filter.
    Filtered,
    /// Listed in `disabled`.
    Disabled,
    /// Discovered, not enabled.
    Available,
}

impl ExtensionState {
    /// Lowercase name used by `plugin.list`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Failed => "failed",
            Self::Filtered => "filtered",
            Self::Disabled => "disabled",
            Self::Available => "available",
        }
    }
}

/// One entry of the resolved order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedExtension {
    /// Extension id.
    pub id: String,
    /// Version.
    pub version: Version,
    /// Earliest phase of its code entries.
    pub phase: Phase,
    /// Manifest hash.
    pub manifest_hash: String,
}

/// The result of [`resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSet {
    /// Dependencies first, grouped by phase, ties by id.
    pub order: Vec<ResolvedExtension>,
    /// State of every discovered id.
    pub states: BTreeMap<String, ExtensionState>,
    /// Findings, sorted.
    pub diagnostics: Vec<Diagnostic>,
    /// Hash of the ordered ids, versions and manifest hashes (hex).
    pub resolved_hash: String,
}

impl ResolvedSet {
    /// Whether any diagnostic is an error.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| d.severity == Severity::Error)
    }

    /// The diagnostic codes recorded for `id`.
    #[must_use]
    pub fn codes_for(&self, id: &str) -> Vec<&'static str> {
        self.diagnostics
            .iter()
            .filter(|d| d.id == id)
            .map(|d| d.code)
            .collect()
    }
}

fn in_scope(list: &Option<Vec<String>>, value: &str) -> bool {
    list.as_ref().is_none_or(|l| l.iter().any(|v| v == value))
}

struct Ctx<'a> {
    nodes: BTreeMap<&'a str, &'a DiscoveredManifest>,
    input: &'a ResolveInput<'a>,
    diags: Vec<Diagnostic>,
    /// Active dependency edges (dependent -> dependencies), in sorted order.
    edges: BTreeMap<&'a str, Vec<&'a str>>,
    own_errors: BTreeSet<&'a str>,
}

impl<'a> Ctx<'a> {
    fn filtered(&self, m: &ExtensionManifest) -> bool {
        !in_scope(&m.platforms, self.input.platform) || !in_scope(&m.profiles, self.input.profile)
    }

    fn edge_active(&self, d: &Dependency) -> bool {
        in_scope(&d.platforms, self.input.platform) && in_scope(&d.profiles, self.input.profile)
    }

    fn error(&mut self, id: &'a str, code: &'static str, message: String) {
        self.own_errors.insert(id);
        self.diags.push(diag(id, code, Severity::Error, message));
    }

    /// Visits `id` and everything it needs, recording errors on the way.
    fn visit(&mut self, id: &'a str, seen: &mut BTreeSet<&'a str>) {
        if !seen.insert(id) {
            return;
        }
        let Some(d) = self.nodes.get(id).copied() else {
            return;
        };
        let m = &d.manifest;
        self.check_node(id, m);
        let mut deps: Vec<&'a str> = Vec::new();
        for dep in &m.dependencies {
            if !self.edge_active(dep) {
                continue;
            }
            let dep_id = dep.id.as_str();
            let found = self.nodes.get(dep_id).copied();
            match found {
                None if dep.optional => self.diags.push(diag(
                    id,
                    "optional_dependency_missing",
                    Severity::Info,
                    format!("optional dependency `{dep_id}` was not discovered"),
                )),
                None => self.error(
                    id,
                    "missing_dependency",
                    format!("required dependency `{dep_id}` was not discovered"),
                ),
                Some(f) => {
                    let disabled = self.input.disabled.iter().any(|x| x == dep_id);
                    let version_ok = dep.version.matches(&f.manifest.version);
                    if disabled {
                        if !dep.optional {
                            self.error(
                                id,
                                "dependency_disabled",
                                format!("dependency `{dep_id}` is disabled"),
                            );
                        }
                    } else if self.filtered(&f.manifest) {
                        if !dep.optional {
                            self.error(
                                id,
                                "missing_dependency",
                                format!(
                                    "dependency `{dep_id}` is filtered out on {}/{}",
                                    self.input.platform, self.input.profile
                                ),
                            );
                        }
                    } else if !version_ok {
                        let msg = format!(
                            "dependency `{dep_id}` is {} but {} is required",
                            f.manifest.version, dep.version
                        );
                        if dep.optional {
                            self.diags
                                .push(diag(id, "version_unsatisfied", Severity::Warning, msg));
                        } else {
                            self.error(id, "version_unsatisfied", msg);
                        }
                    } else {
                        deps.push(dep_id);
                    }
                }
            }
        }
        deps.sort_unstable();
        deps.dedup();
        // Phase inversion: a dependency may not start later than its dependent.
        for dep_id in &deps {
            if let Some(f) = self.nodes.get(dep_id).copied()
                && f.manifest.phase() > m.phase()
            {
                self.error(
                    id,
                    "phase_inversion",
                    format!(
                        "`{dep_id}` loads in phase {} after `{id}` in phase {}",
                        f.manifest.phase().name(),
                        m.phase().name()
                    ),
                );
            }
        }
        self.edges.insert(id, deps.clone());
        for dep_id in deps {
            self.visit(dep_id, seen);
        }
    }

    fn check_node(&mut self, id: &'a str, m: &'a ExtensionManifest) {
        if self.input.reserved_ids.iter().any(|r| r == &m.id) {
            self.error(
                id,
                "reserved_id",
                format!("`{}` is an engine agent namespace", m.id),
            );
        }
        if let Some(min) = &m.engine_min
            && self.input.engine_version < min
        {
            self.error(
                id,
                "engine_incompatible",
                format!("engine {} is below min_version {min}", self.input.engine_version),
            );
        }
        if let Some(max) = &m.engine_max
            && self.input.engine_version > max
        {
            self.error(
                id,
                "engine_incompatible",
                format!("engine {} is above max_version {max}", self.input.engine_version),
            );
        }
        let granted: BTreeSet<&str> = self
            .input
            .grants
            .get(id)
            .map(|g| g.iter().map(String::as_str).collect())
            .unwrap_or_default();
        for cap in &m.capabilities {
            if !KNOWN_CAPABILITIES.contains(&cap.as_str()) {
                self.error(id, "unknown_capability", format!("unknown capability `{cap}`"));
            } else if m.load_capabilities.contains(cap)
                && !IMPLICIT_CAPABILITIES.contains(&cap.as_str())
                && !granted.contains(cap.as_str())
            {
                self.error(
                    id,
                    "capability_not_granted",
                    format!("capability `{cap}` is requested but not granted"),
                );
            }
        }
        if !self.input.dynamic_code
            && m.code
                .iter()
                .any(|c| c.crate_name.is_none() && c.library.is_some())
        {
            self.error(
                id,
                "not_supported",
                format!(
                    "`{id}` has only a prebuilt library and {} cannot load code at run time",
                    self.input.platform
                ),
            );
        }
    }
}

fn closure<'a>(edges: &BTreeMap<&'a str, Vec<&'a str>>, root: &'a str) -> BTreeSet<&'a str> {
    reach(edges, std::iter::once(root))
}

/// Everything reachable from `starts` over `edges`, starts included, in one walk.
fn reach<'a>(
    edges: &BTreeMap<&'a str, Vec<&'a str>>,
    starts: impl IntoIterator<Item = &'a str>,
) -> BTreeSet<&'a str> {
    let mut out = BTreeSet::new();
    let mut stack: Vec<&'a str> = starts.into_iter().collect();
    while let Some(n) = stack.pop() {
        if out.insert(n)
            && let Some(ds) = edges.get(n)
        {
            stack.extend(ds.iter().copied());
        }
    }
    out
}

/// Finds dependency cycles in `edges`, each as the member list in walk order.
fn find_cycles<'a>(edges: &BTreeMap<&'a str, Vec<&'a str>>) -> Vec<Vec<&'a str>> {
    fn walk<'a>(
        n: &'a str,
        edges: &BTreeMap<&'a str, Vec<&'a str>>,
        stack: &mut Vec<&'a str>,
        done: &mut BTreeSet<&'a str>,
        found: &mut Vec<Vec<&'a str>>,
    ) {
        if let Some(at) = stack.iter().position(|s| *s == n) {
            found.push(stack[at..].to_vec());
            return;
        }
        if done.contains(n) {
            return;
        }
        stack.push(n);
        for d in edges.get(n).into_iter().flatten() {
            walk(d, edges, stack, done, found);
        }
        stack.pop();
        done.insert(n);
    }
    let mut found = Vec::new();
    let mut done = BTreeSet::new();
    for n in edges.keys() {
        walk(n, edges, &mut Vec::new(), &mut done, &mut found);
    }
    found
}

/// Resolves the enabled set (spec § 2.5). Deterministic: the same inputs give the same
/// `resolved_hash` whatever the discovery order, as long as the first manifest of each duplicate
/// id is the same.
#[must_use]
pub fn resolve(input: &ResolveInput<'_>) -> ResolvedSet {
    let mut ctx = Ctx {
        nodes: BTreeMap::new(),
        input,
        diags: Vec::new(),
        edges: BTreeMap::new(),
        own_errors: BTreeSet::new(),
    };
    let mut states: BTreeMap<String, ExtensionState> = BTreeMap::new();
    let mut stop = false;
    for d in input.discovered {
        let id = d.manifest.id.as_str();
        match ctx.nodes.get(id) {
            None => {
                ctx.nodes.insert(id, d);
            }
            Some(first) if first.path != d.path => {
                // Either copy may be the one asking to load: dropping the second silently would
                // leave an `auto_enable` engine extension off because of an idle project copy.
                let named = input.enabled.iter().any(|e| e == id)
                    || first.manifest.auto_enable
                    || d.manifest.auto_enable;
                let msg = format!("`{}` and `{}` both declare `{id}`", first.path, d.path);
                if named {
                    stop = true;
                    ctx.diags.push(diag(id, "duplicate_id", Severity::Error, msg));
                } else {
                    ctx.diags.push(diag(id, "duplicate_id", Severity::Warning, msg));
                }
            }
            Some(_) => {}
        }
    }
    let ids: Vec<&str> = ctx.nodes.keys().copied().collect();
    if stop {
        for id in &ids {
            states.insert((*id).to_owned(), ExtensionState::Failed);
        }
        let mut diagnostics = ctx.diags;
        diagnostics.sort();
        return ResolvedSet {
            order: Vec::new(),
            states,
            diagnostics,
            resolved_hash: hash_of(&[]),
        };
    }

    // Roots and per-extension states.
    let mut roots: Vec<&str> = Vec::new();
    for id in &ids {
        let m = &ctx.nodes[id].manifest;
        let listed = input.enabled.iter().any(|e| e == id);
        if input.disabled.iter().any(|x| x == id) {
            states.insert((*id).to_owned(), ExtensionState::Disabled);
        } else if !(listed || m.auto_enable) {
            states.insert((*id).to_owned(), ExtensionState::Available);
        } else if ctx.filtered(m) {
            states.insert((*id).to_owned(), ExtensionState::Filtered);
            ctx.diags.push(diag(
                id,
                "filtered",
                Severity::Info,
                format!("not loaded on {}/{}", input.platform, input.profile),
            ));
        } else {
            roots.push(id);
        }
    }
    for e in input.enabled {
        if !ctx.nodes.contains_key(e.as_str()) {
            ctx.diags.push(diag(
                e,
                "not_found",
                Severity::Error,
                format!("`{e}` is enabled but no manifest with that id was found"),
            ));
        }
    }

    let mut seen = BTreeSet::new();
    for r in &roots {
        ctx.visit(r, &mut seen);
    }
    let cycles = find_cycles(&ctx.edges);
    let mut in_cycle: BTreeSet<&str> = BTreeSet::new();
    let mut reported: BTreeSet<Vec<&str>> = BTreeSet::new();
    for c in &cycles {
        let mut key = c.clone();
        key.sort_unstable();
        if reported.insert(key) {
            let path = c
                .iter()
                .chain(c.first())
                .copied()
                .collect::<Vec<_>>()
                .join(" -> ");
            ctx.diags.push(diag(
                c[0],
                "dependency_cycle",
                Severity::Error,
                format!("cycle: {path}"),
            ));
        }
        in_cycle.extend(c.iter().copied());
    }

    let mut failed_roots: BTreeSet<&str> = BTreeSet::new();
    let mut bad: BTreeSet<&str> = ctx.own_errors.clone();
    bad.extend(in_cycle.iter().copied());
    let mut ok_roots: Vec<&str> = Vec::new();
    // A root fails when something it needs is bad: the walk from the bad ones, against the edges.
    let mut needed_by: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (id, deps) in &ctx.edges {
        for dep in deps {
            needed_by.entry(*dep).or_default().push(*id);
        }
    }
    let tainted = reach(&needed_by, bad.iter().copied());
    for r in &roots {
        if tainted.contains(r) {
            failed_roots.insert(r);
        } else {
            ok_roots.push(r);
        }
    }

    // Pairwise rules across what is left; failing the later id's roots until stable.
    loop {
        let live = reach(&ctx.edges, ok_roots.iter().copied());
        let Some((loser, d)) = pair_violation(&ctx, &live) else {
            break;
        };
        ctx.diags.push(d);
        bad.insert(loser);
        let (still_ok, now_failed): (Vec<&str>, Vec<&str>) = ok_roots
            .iter()
            .partition(|r| !closure(&ctx.edges, r).contains(loser));
        failed_roots.extend(now_failed);
        ok_roots = still_ok;
    }

    let live = reach(&ctx.edges, ok_roots.iter().copied());
    let order = topo_order(&ctx, &live);
    for id in &live {
        states.insert((*id).to_owned(), ExtensionState::Active);
    }
    for id in &failed_roots {
        states.insert((*id).to_owned(), ExtensionState::Failed);
    }
    for id in &bad {
        if !live.contains(id) {
            states.insert((*id).to_owned(), ExtensionState::Failed);
        }
    }
    for id in &ids {
        states
            .entry((*id).to_owned())
            .or_insert(ExtensionState::Available);
    }
    let resolved: Vec<ResolvedExtension> = order
        .iter()
        .map(|id| {
            let d = ctx.nodes[id];
            ResolvedExtension {
                id: (*id).to_owned(),
                version: d.manifest.version.clone(),
                phase: d.manifest.phase(),
                manifest_hash: d.hash.clone(),
            }
        })
        .collect();
    let mut diagnostics = ctx.diags;
    diagnostics.sort();
    diagnostics.dedup();
    let resolved_hash = hash_of(&resolved);
    ResolvedSet {
        order: resolved,
        states,
        diagnostics,
        resolved_hash,
    }
}

/// The first conflict or `provides` clash among `live` (sorted), with the id that loses.
fn pair_violation<'a>(ctx: &Ctx<'a>, live: &BTreeSet<&'a str>) -> Option<(&'a str, Diagnostic)> {
    // Most sets declare neither, and the pairs below are quadratic.
    let declares = |id: &&str| {
        let m = &ctx.nodes[*id].manifest;
        !m.conflicts.is_empty() || !m.provides.is_empty()
    };
    if !live.iter().any(declares) {
        return None;
    }
    let ids: Vec<&'a str> = live.iter().copied().collect();
    for (i, a) in ids.iter().enumerate() {
        for b in &ids[i + 1..] {
            let (ma, mb) = (&ctx.nodes[a].manifest, &ctx.nodes[b].manifest);
            if ma.conflicts.iter().any(|c| c == b) || mb.conflicts.iter().any(|c| c == a) {
                return Some((
                    b,
                    diag(
                        b,
                        "conflict",
                        Severity::Error,
                        format!("`{a}` and `{b}` conflict"),
                    ),
                ));
            }
            let replaced = ma.replaces.iter().any(|r| r == b) || mb.replaces.iter().any(|r| r == a);
            if !replaced && let Some(tok) = ma.provides.iter().find(|p| mb.provides.contains(p)) {
                return Some((
                    b,
                    diag(
                        b,
                        "provides_clash",
                        Severity::Error,
                        format!("`{a}` and `{b}` both provide `{tok}`"),
                    ),
                ));
            }
        }
    }
    None
}

/// Kahn's algorithm taking the smallest `(phase, id)` among the ready nodes.
fn topo_order<'a>(ctx: &Ctx<'a>, live: &BTreeSet<&'a str>) -> Vec<&'a str> {
    let mut waiting: BTreeMap<&str, usize> = BTreeMap::new();
    let mut dependents: BTreeMap<&str, Vec<&'a str>> = BTreeMap::new();
    for n in live {
        let deps = ctx.edges.get(n).map_or(&[][..], Vec::as_slice);
        let mut count = 0;
        for dep in deps.iter().filter(|d| live.contains(*d)) {
            dependents.entry(*dep).or_default().push(n);
            count += 1;
        }
        waiting.insert(n, count);
    }
    let key = |n: &'a str| (ctx.nodes[n].manifest.phase(), n);
    let mut ready: BTreeSet<_> = live
        .iter()
        .filter(|n| waiting[**n] == 0)
        .map(|n| key(n))
        .collect();
    let mut order = Vec::with_capacity(live.len());
    while let Some((_, n)) = ready.pop_first() {
        order.push(n);
        for dependent in dependents.get(n).into_iter().flatten() {
            if let Some(count) = waiting.get_mut(dependent) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    ready.insert(key(dependent));
                }
            }
        }
    }
    order
}

fn hash_of(order: &[ResolvedExtension]) -> String {
    let mut h = blake3::Hasher::new();
    for e in order {
        h.update(e.id.as_bytes());
        h.update(&[0]);
        h.update(e.version.to_string().as_bytes());
        h.update(&[0]);
        h.update(e.manifest_hash.as_bytes());
        h.update(&[0]);
    }
    h.finalize().to_hex().to_string()
}
