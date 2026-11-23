// SPDX-License-Identifier: MPL-2.0
//! Asset validators (ADR-1568): a [`Validator`] trait, a [`ValidatorRegistry`], six built-in checks and
//! a [`ValidationReport`] that serializes to JSON or RON.
//!
//! A validator looks at one [`AssetEntry`] with the whole [`AssetSet`] (and its [`DependencyGraph`])
//! available through a [`ValidationContext`], and answers [`Outcome::Pass`], [`Outcome::Fail`] or
//! [`Outcome::NotValidated`] (the check does not apply to the asset, or the set lacks the data it needs)
//! plus messages. Nothing reads the disk; the same set gives the same report, ordered by asset path and
//! then validator name.
//!
//! The built-ins: [`MissingReferences`], [`NamingRules`], [`TextureSizeLimits`], [`DuplicateIds`],
//! [`OrphanedAssets`] and [`MeshTriangleBudget`], configured by [`ValidationConfig`].

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::asset_graph::{AssetEntry, AssetKind, AssetSet, DependencyGraph, SizeRollup};
use crate::lod_classes::glob_match;

/// The `format_version` written into every serialized [`ValidationReport`].
/// Source-lint evidence uses `source_lint`, not this report (ADR-8248).
pub const VALIDATION_REPORT_FORMAT_VERSION: u32 = 1;

/// The verdict of one validator on one asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The asset satisfies the check.
    Pass,
    /// The asset breaks the check.
    Fail,
    /// The check does not apply, or the data it needs is missing.
    NotValidated,
}

impl Outcome {
    /// Lowercase label (`pass`, `fail`, `not_validated`).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::NotValidated => "not_validated",
        }
    }
}

/// How serious a [`Finding`] is (spec 32 § 4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Worth knowing; never fails an asset.
    #[serde(alias = "Info")]
    Info,
    /// A problem that fails the asset only when the run sets `warnings_fail` (or the CLI runs `--strict`).
    #[serde(alias = "Warning")]
    Warning,
    /// A problem that fails the asset.
    #[serde(alias = "Error")]
    Error,
}

impl Severity {
    /// Lowercase label (`info`, `warning`, `error`).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// One result line of a validator: a stable code, a severity, the text and an optional hint.
/// Source-lint findings are a different row (`source_lint::LintFinding`, ADR-8248).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Stable code: lowercase ascii letters, digits and `_`. Empty in a validator's own verdict means
    /// "use the validator's name"; the runner fills it in.
    pub code: String,
    /// How serious it is.
    pub severity: Severity,
    /// The human-readable text.
    pub text: String,
    /// What to do about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Finding {
    /// A finding without a hint.
    #[must_use]
    pub fn new(code: impl Into<String>, severity: Severity, text: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            severity,
            text: text.into(),
            hint: None,
        }
    }

    /// An `Error` finding.
    #[must_use]
    pub fn error(code: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(code, Severity::Error, text)
    }

    /// Attach a hint.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// What a [`Validator`] returns for one asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// Pass, fail or not validated.
    pub outcome: Outcome,
    /// Human-readable reasons (one per problem; may explain a pass or a skip too). Equals the
    /// findings' texts when there are findings.
    pub messages: Vec<String>,
    /// Coded findings (spec 32 § 4.3); empty for a verdict that has none.
    pub findings: Vec<Finding>,
}

impl Verdict {
    /// A pass with no message.
    #[must_use]
    pub fn pass() -> Self {
        Self {
            outcome: Outcome::Pass,
            messages: Vec::new(),
            findings: Vec::new(),
        }
    }

    /// A failure with the given messages; each becomes an `Error` finding whose code is the
    /// validator's name.
    #[must_use]
    pub fn fail(messages: Vec<String>) -> Self {
        let findings = messages
            .iter()
            .map(|m| Finding::error(String::new(), m.clone()))
            .collect();
        Self {
            outcome: Outcome::Fail,
            messages,
            findings,
        }
    }

    /// A failure carrying the given findings.
    #[must_use]
    pub fn fail_with(findings: Vec<Finding>) -> Self {
        Self {
            outcome: Outcome::Fail,
            messages: findings.iter().map(|f| f.text.clone()).collect(),
            findings,
        }
    }

    /// A pass that may carry `Warning` or `Info` findings.
    #[must_use]
    pub fn pass_with(findings: Vec<Finding>) -> Self {
        Self {
            outcome: Outcome::Pass,
            messages: findings.iter().map(|f| f.text.clone()).collect(),
            findings,
        }
    }

    /// Fail when any finding is an `Error`, else pass carrying the findings.
    #[must_use]
    pub fn from_findings(findings: Vec<Finding>) -> Self {
        if findings.iter().any(|f| f.severity == Severity::Error) {
            Self::fail_with(findings)
        } else {
            Self::pass_with(findings)
        }
    }

    /// Not validated, with the reason.
    #[must_use]
    pub fn not_validated(reason: impl Into<String>) -> Self {
        Self {
            outcome: Outcome::NotValidated,
            messages: vec![reason.into()],
            findings: Vec::new(),
        }
    }

    /// Pass when `messages` is empty, else fail with them.
    #[must_use]
    pub fn from_problems(messages: Vec<String>) -> Self {
        if messages.is_empty() {
            Self::pass()
        } else {
            Self::fail(messages)
        }
    }
}

/// One row of a [`ValidationReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationResult {
    /// The validator's name.
    pub validator: String,
    /// The asset path.
    pub asset: String,
    /// The verdict.
    pub outcome: Outcome,
    /// The messages.
    pub messages: Vec<String>,
    /// The coded findings (spec 32 § 4.3); older reports have none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
}

/// Everything a validator may look at besides the asset itself.
#[derive(Debug)]
pub struct ValidationContext<'a> {
    /// The set under validation.
    pub set: &'a AssetSet,
    /// Its dependency graph.
    pub graph: DependencyGraph,
    ids: BTreeMap<String, Vec<String>>,
    path_counts: BTreeMap<String, usize>,
    /// Exact file paths that share an ASCII case-fold with a different file path.
    /// The value is the other paths, sorted. Absent when the path collides with nothing.
    case_collisions: BTreeMap<String, Vec<String>>,
    roots: Vec<String>,
    /// Roots that name no asset of the set (a typo in `roots`): they reach nothing.
    unknown_roots: Vec<String>,
    reachable: BTreeSet<String>,
}

impl<'a> ValidationContext<'a> {
    /// Index `set` for the validators.
    #[must_use]
    pub fn new(set: &'a AssetSet) -> Self {
        let graph = DependencyGraph::build(set);
        let mut ids: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut path_counts: BTreeMap<String, usize> = BTreeMap::new();
        for asset in &set.assets {
            *path_counts.entry(asset.path.clone()).or_default() += 1;
            if let Some(id) = &asset.id {
                ids.entry(id.clone()).or_default().push(asset.path.clone());
            }
        }
        let case_collisions = index_case_collisions(&set.assets, &path_counts);
        let roots = set.effective_roots();
        let unknown_roots = roots
            .iter()
            .filter(|r| !path_counts.contains_key(*r))
            .cloned()
            .collect();
        let reachable = graph.reachable_from(roots.iter().map(String::as_str));
        Self {
            set,
            graph,
            ids,
            path_counts,
            case_collisions,
            roots,
            unknown_roots,
            reachable,
        }
    }

    /// The paths of every asset that carries `id` (in set order).
    #[must_use]
    pub fn owners_of_id(&self, id: &str) -> &[String] {
        self.ids.get(id).map_or(&[], Vec::as_slice)
    }

    /// How many entries share `path`.
    #[must_use]
    pub fn path_count(&self, path: &str) -> usize {
        self.path_counts.get(path).copied().unwrap_or(0)
    }

    /// The effective roots of the orphan check.
    #[must_use]
    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    /// The effective roots that name no asset of the set (they reach nothing).
    #[must_use]
    pub fn unknown_roots(&self) -> &[String] {
        &self.unknown_roots
    }

    /// Whether `path` is reachable from the roots.
    #[must_use]
    pub fn is_reachable(&self, path: &str) -> bool {
        self.reachable.contains(path)
    }
}

/// One check over assets. Implementations must be deterministic and must not touch the disk.
/// Source-byte evidence is `source_lint` (ADR-8248), not this trait.
pub trait Validator: Send + Sync {
    /// The unique registry name (lowercase ascii, digits and `_`).
    fn name(&self) -> &str;

    /// One-line description for listings.
    fn description(&self) -> &str;

    /// Judge `asset`.
    fn validate(&self, ctx: &ValidationContext<'_>, asset: &AssetEntry) -> Verdict;
}

/// Limits and rules of the built-in validators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ValidationConfig {
    /// Longest allowed texture side in pixels.
    pub max_texture_dimension: u32,
    /// Whether both texture sides must be powers of two.
    pub texture_power_of_two: bool,
    /// Most triangles one mesh may have.
    pub mesh_triangle_budget: u64,
    /// Longest allowed asset path in bytes.
    pub max_path_len: usize,
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            max_texture_dimension: 4096,
            texture_power_of_two: false,
            mesh_triangle_budget: 200_000,
            max_path_len: 200,
        }
    }
}

/// Fails an asset that references a path that is not in the set.
#[derive(Debug, Clone, Copy, Default)]
pub struct MissingReferences;

impl Validator for MissingReferences {
    fn name(&self) -> &str {
        "missing_references"
    }

    fn description(&self) -> &str {
        "every referenced path exists in the set"
    }

    fn validate(&self, ctx: &ValidationContext<'_>, asset: &AssetEntry) -> Verdict {
        // A UI layout's missing pictures are `ui_image_missing` ([`UiImageMissing`]); its missing
        // component `use` documents stay here (ADR-3147, ADR-3148).
        let layout = asset.tags.contains_key(crate::registry_bridge::UI_LAYOUT_TAG);
        let problems = ctx
            .graph
            .missing_references(&asset.path)
            .into_iter()
            .filter(|p| !layout || is_ui_layout_path(p))
            .map(|p| {
                Finding::error(
                    "validate_reference_missing",
                    format!("references `{p}`, which is not in the asset set"),
                )
            })
            .collect::<Vec<_>>();
        if problems.is_empty() {
            Verdict::pass()
        } else {
            Verdict::fail_with(problems)
        }
    }
}

/// A reference to another `.ui.ron` (a component `use`), not a picture.
fn is_ui_layout_path(path: &str) -> bool {
    path.ends_with(".ui.ron")
}

/// Fails a UI layout (`.ui.ron`, [`crate::registry_bridge::UI_LAYOUT_TAG`]) whose `image` or `mask`
/// names an asset path that is not in the set (code `ui_image_missing`, ADR-3148), and one that does
/// not parse (code `ui_layout_invalid`): at run time such a picture draws the fallback.
#[derive(Debug, Clone, Copy, Default)]
pub struct UiImageMissing;

impl Validator for UiImageMissing {
    fn name(&self) -> &str {
        "ui_image_missing"
    }

    fn description(&self) -> &str {
        "every asset path a UI layout's images and masks name exists in the set"
    }

    fn validate(&self, ctx: &ValidationContext<'_>, asset: &AssetEntry) -> Verdict {
        if !asset.tags.contains_key(crate::registry_bridge::UI_LAYOUT_TAG) {
            return Verdict::not_validated("not a UI layout");
        }
        if let Some(error) = asset.tags.get(crate::registry_bridge::UI_LAYOUT_ERROR_TAG) {
            return Verdict::fail_with(vec![Finding::error(
                "ui_layout_invalid",
                format!("the layout does not load: {error}"),
            )]);
        }
        let problems = ctx
            .graph
            .missing_references(&asset.path)
            .into_iter()
            .filter(|p| !is_ui_layout_path(p))
            .map(|p| {
                Finding::error(
                    "ui_image_missing",
                    format!("an image or mask names `{p}`, which no cooked asset matches"),
                )
            })
            .collect::<Vec<_>>();
        if problems.is_empty() {
            Verdict::pass()
        } else {
            Verdict::fail_with(problems)
        }
    }
}

/// Path naming rules: ASCII letters in either case, digits, `_`, `-` and `.` in every
/// `/`-separated segment, no empty or hidden segments, no `\`, no Windows device name, no second
/// file path that differs only by ASCII letter case, and a length limit. A cooker sub-asset path
/// `<source>#<label>` whose source is an asset of the set has only its source part checked.
#[derive(Debug, Clone, Copy)]
pub struct NamingRules {
    max_path_len: usize,
}

impl NamingRules {
    /// Rules with the path length limit of `config`.
    #[must_use]
    pub fn new(config: &ValidationConfig) -> Self {
        Self {
            max_path_len: config.max_path_len,
        }
    }
}

impl Validator for NamingRules {
    fn name(&self) -> &str {
        "naming_rules"
    }

    fn description(&self) -> &str {
        "paths use ASCII letters, digits, `_`, `-` and `.` in any letter case, with no empty or hidden segments and no two paths that differ only by case (a sub-asset's generated `#` label is not a file name)"
    }

    fn validate(&self, ctx: &ValidationContext<'_>, asset: &AssetEntry) -> Verdict {
        let full = asset.path.as_str();
        let mut problems = Vec::new();
        if full.is_empty() {
            return coded(vec!["the path is empty".into()], "validate_name_invalid");
        }
        // `<source>#<label>` is a cooker-generated sub-asset (terrain tiles, meshlets, stream
        // cells): only its source part is a file name. A `#` after a path that is no asset of the
        // set stays part of a (bad) file name.
        if let Some((source, label)) = full.split_once('#')
            && ctx.path_counts.contains_key(source)
            && label.is_empty()
        {
            problems.push("the sub-asset label after `#` is empty".into());
        }
        let path = naming_file_path(full, &ctx.path_counts);
        if full.len() > self.max_path_len {
            problems.push(format!(
                "the path is {} bytes long, over the limit of {}",
                full.len(),
                self.max_path_len
            ));
        }
        if path.contains('\\') {
            problems.push("the path contains `\\`; use `/` separators".into());
        }
        if path.starts_with('/') {
            problems.push("the path is absolute; it must be project-relative".into());
        }
        for segment in path.strip_prefix('/').unwrap_or(path).split('/') {
            if segment.is_empty() {
                problems.push("the path has an empty segment (`//` or a trailing `/`)".into());
            } else if segment.starts_with('.') {
                problems.push(format!("segment `{segment}` starts with `.`"));
            } else if let Some(bad) = segment.chars().find(|c| !is_name_char(*c)) {
                problems.push(format!(
                    "segment `{segment}` contains `{}`; allowed are ASCII letters, digits, `_`, `-` and `.`",
                    bad.escape_default()
                ));
            } else if segment.ends_with('.') {
                problems.push(format!(
                    "segment `{segment}` ends with `.`, which Windows strips from file names"
                ));
            } else if is_windows_device_name(segment) {
                problems.push(format!(
                    "segment `{segment}` is a Windows device name; such a file cannot be created on Windows"
                ));
            }
        }
        let collision = ctx
            .case_collisions
            .get(path)
            .map(|others| case_collision_text(path, others));
        naming_verdict(problems, collision)
    }
}

/// A verdict whose every message is an `Error` finding with `code` (pass when there are none).
fn coded(messages: Vec<String>, code: &str) -> Verdict {
    if messages.is_empty() {
        return Verdict::pass();
    }
    Verdict::fail_with(messages.into_iter().map(|m| Finding::error(code, m)).collect())
}

/// `validate_name_invalid` for `problems`, plus `validate_name_case_collision` when `collision` is set.
fn naming_verdict(problems: Vec<String>, collision: Option<String>) -> Verdict {
    let mut findings: Vec<Finding> = problems
        .into_iter()
        .map(|m| Finding::error("validate_name_invalid", m))
        .collect();
    if let Some(text) = collision {
        findings.push(Finding::error("validate_name_case_collision", text));
    }
    if findings.is_empty() {
        Verdict::pass()
    } else {
        Verdict::fail_with(findings)
    }
}

/// File paths in `assets` whose ASCII case-fold matches a different file path.
///
/// The file path is the source before a cooker `#` label when that source is in `path_counts`,
/// and the whole path otherwise. Exact duplicates are not a collision: the paths must differ.
///
/// A set whose file paths contain no ASCII uppercase letter is already one case, so it cannot
/// hold such a pair. Returning before the case-fold copies keeps that run at the same allocation
/// count as before this index existed (the asset-database count budget, ADR-2704).
fn index_case_collisions(
    assets: &[AssetEntry],
    path_counts: &BTreeMap<String, usize>,
) -> BTreeMap<String, Vec<String>> {
    let any_uppercase = assets.iter().any(|asset| {
        naming_file_path(&asset.path, path_counts)
            .bytes()
            .any(|byte| byte.is_ascii_uppercase())
    });
    if !any_uppercase {
        return BTreeMap::new();
    }
    let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for asset in assets {
        let file = naming_file_path(&asset.path, path_counts);
        groups
            .entry(file.to_ascii_lowercase())
            .or_default()
            .insert(file.to_owned());
    }
    let mut collisions = BTreeMap::new();
    for group in groups.into_values() {
        if group.len() < 2 {
            continue;
        }
        let members: Vec<String> = group.into_iter().collect();
        for path in &members {
            let others: Vec<String> = members.iter().filter(|other| *other != path).cloned().collect();
            collisions.insert(path.clone(), others);
        }
    }
    collisions
}

/// The file-name portion of `full`: the source before `#` when that source is an asset of the set.
fn naming_file_path<'a>(full: &'a str, path_counts: &BTreeMap<String, usize>) -> &'a str {
    match full.split_once('#') {
        Some((source, _)) if path_counts.contains_key(source) => source,
        _ => full,
    }
}

fn case_collision_text(path: &str, others: &[String]) -> String {
    let shown: Vec<&str> = others
        .iter()
        .take(MAX_LISTED_OWNERS)
        .map(String::as_str)
        .collect();
    let listed = join_quoted(&shown);
    let extra = others.len().saturating_sub(MAX_LISTED_OWNERS);
    if extra == 0 {
        format!(
            "`{path}` differs only by letter case from {listed}; a case-insensitive file system cannot hold both"
        )
    } else {
        format!(
            "`{path}` differs only by letter case from {listed} and {extra} more; a case-insensitive file system cannot hold both"
        )
    }
}

/// Whether `segment` is reserved on Windows, in any ASCII case: `CON`, `PRN`, `AUX`, `NUL`,
/// `COM1`-`COM9` or `LPT1`-`LPT9`, alone or before an extension (`nul.glb`, `CON.tar.gz`).
fn is_windows_device_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or(segment);
    if ["con", "prn", "aux", "nul"]
        .iter()
        .any(|name| stem.eq_ignore_ascii_case(name))
    {
        return true;
    }
    let bytes = stem.as_bytes();
    if bytes.len() != 4 {
        return false;
    }
    let head = &bytes[..3];
    (head.eq_ignore_ascii_case(b"com") || head.eq_ignore_ascii_case(b"lpt"))
        && bytes[3].is_ascii_digit()
        && bytes[3] != b'0'
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphabetic() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.')
}

/// Texture side limit (and optionally power-of-two sides).
#[derive(Debug, Clone, Copy)]
pub struct TextureSizeLimits {
    max_dimension: u32,
    power_of_two: bool,
}

impl TextureSizeLimits {
    /// Limits from `config`.
    #[must_use]
    pub fn new(config: &ValidationConfig) -> Self {
        Self {
            max_dimension: config.max_texture_dimension,
            power_of_two: config.texture_power_of_two,
        }
    }
}

impl Validator for TextureSizeLimits {
    fn name(&self) -> &str {
        "texture_size_limits"
    }

    fn description(&self) -> &str {
        "textures stay within the maximum side length (and power-of-two sides when required)"
    }

    fn validate(&self, _ctx: &ValidationContext<'_>, asset: &AssetEntry) -> Verdict {
        if asset.kind != AssetKind::Texture {
            return Verdict::not_validated("not a texture");
        }
        let Some([w, h]) = asset.texture_size else {
            return Verdict::not_validated("the texture has no recorded size");
        };
        let mut problems = Vec::new();
        if w == 0 || h == 0 {
            problems.push(format!("the texture is {w}x{h}; a side is zero"));
        }
        if w > self.max_dimension || h > self.max_dimension {
            problems.push(format!(
                "the texture is {w}x{h}, over the limit of {} per side",
                self.max_dimension
            ));
        }
        if self.power_of_two && w != 0 && h != 0 && !(w.is_power_of_two() && h.is_power_of_two()) {
            problems.push(format!(
                "the texture is {w}x{h}; both sides must be powers of two"
            ));
        }
        Verdict::from_problems(problems)
    }
}

/// Fails assets that share an id with another asset, or a path with another entry.
#[derive(Debug, Clone, Copy, Default)]
pub struct DuplicateIds;

impl Validator for DuplicateIds {
    fn name(&self) -> &str {
        "duplicate_ids"
    }

    fn description(&self) -> &str {
        "asset ids and paths are unique within the set"
    }

    fn validate(&self, ctx: &ValidationContext<'_>, asset: &AssetEntry) -> Verdict {
        let mut problems = Vec::new();
        if ctx.path_count(&asset.path) > 1 {
            problems.push(format!(
                "the path appears {} times in the set",
                ctx.path_count(&asset.path)
            ));
        }
        if let Some(id) = &asset.id {
            let owners = ctx.owners_of_id(id);
            // The asset is one of its id's owners: once, unless its path is listed twice (already
            // an error above). Counting this way keeps one shared id across N assets O(N), not
            // O(N^2), and the message names at most `MAX_LISTED_OWNERS` of the others.
            let own = if ctx.path_count(&asset.path) == 1 {
                1
            } else {
                owners.iter().filter(|p| **p == asset.path).count()
            };
            let total = owners.len().saturating_sub(own);
            let listed: Vec<&str> = owners
                .iter()
                .map(String::as_str)
                .filter(|p| *p != asset.path)
                .take(MAX_LISTED_OWNERS)
                .collect();
            if total > 0 {
                let more = total.saturating_sub(listed.len());
                let tail = if more > 0 {
                    format!(" and {more} more")
                } else {
                    String::new()
                };
                problems.push(format!(
                    "id `{id}` is also used by {}{tail}",
                    join_quoted(&listed)
                ));
            }
        }
        if problems.is_empty() && asset.id.is_none() {
            return Verdict::not_validated("the asset has no id");
        }
        coded(problems, "validate_id_duplicate")
    }
}

/// Most other owners a [`DuplicateIds`] message names.
const MAX_LISTED_OWNERS: usize = 8;

fn join_quoted(paths: &[&str]) -> String {
    paths
        .iter()
        .map(|p| format!("`{p}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The tag [`AssetSet::from_registry`] sets on localized variants and their bases: the cooked type path.
pub const COOKED_TYPE_TAG: &str = "cooked_type";

/// Checks a localized variant (`localized/<L>/<rel>`, tagged `localized_of`) against the asset it
/// replaces (spec 36 § 6): another cooked type is an error, because the loader ignores such a variant
/// and falls back to the base; no base is a warning, because the other locales then have nothing.
/// Not validated for an asset that is no variant.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalizedTwin;

impl Validator for LocalizedTwin {
    fn name(&self) -> &str {
        "localized_twin"
    }

    fn description(&self) -> &str {
        "a localized variant has the cooked type of the asset it replaces"
    }

    fn validate(&self, ctx: &ValidationContext<'_>, asset: &AssetEntry) -> Verdict {
        let Some(base_path) = asset.tags.get("localized_of") else {
            return Verdict::not_validated("the asset is not a localized variant");
        };
        let Some(base) = ctx.set.get(base_path) else {
            return Verdict::from_findings(vec![
                Finding::new(
                    "validate_localized_no_base",
                    Severity::Warning,
                    format!("the variant replaces `{base_path}`, which is not in the set"),
                )
                .with_hint("add the base asset, or the other locales have nothing to fall back to"),
            ]);
        };
        let same_type = asset.kind == base.kind
            && match (asset.tags.get(COOKED_TYPE_TAG), base.tags.get(COOKED_TYPE_TAG)) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            };
        if same_type {
            return Verdict::pass();
        }
        Verdict::from_findings(vec![
            Finding::error(
                "validate_localized_type_mismatch",
                format!("the variant's cooked type differs from `{base_path}`; the loader ignores it"),
            )
            .with_hint("give the variant the same asset type as its base"),
        ])
    }
}

/// Fails assets that no root reaches. Not validated when the set has no roots (no scenes and no
/// explicit `roots`), since "orphaned" has no meaning then.
#[derive(Debug, Clone, Copy, Default)]
pub struct OrphanedAssets;

impl Validator for OrphanedAssets {
    fn name(&self) -> &str {
        "orphaned_assets"
    }

    fn description(&self) -> &str {
        "every asset is reachable from a root (a scene or a listed root)"
    }

    fn validate(&self, ctx: &ValidationContext<'_>, asset: &AssetEntry) -> Verdict {
        if ctx.roots().is_empty() {
            return Verdict::not_validated("the set has no roots (no scenes and no explicit roots)");
        }
        if ctx.is_reachable(&asset.path) {
            Verdict::pass()
        } else {
            let referrers = ctx.graph.referrers(&asset.path);
            let note = if referrers.is_empty() {
                "nothing references it".to_owned()
            } else {
                format!("only unreachable assets reference it ({})", referrers.join(", "))
            };
            // A misspelled root makes everything under it look orphaned: say so.
            let unknown = ctx.unknown_roots();
            let roots_note = if unknown.is_empty() {
                String::new()
            } else {
                let listed: Vec<&str> = unknown
                    .iter()
                    .take(MAX_LISTED_OWNERS)
                    .map(String::as_str)
                    .collect();
                format!("; roots not in the set: {}", join_quoted(&listed))
            };
            coded(
                vec![format!("not reachable from any root: {note}{roots_note}")],
                "validate_asset_orphaned",
            )
        }
    }
}

/// Mesh triangle budget.
#[derive(Debug, Clone, Copy)]
pub struct MeshTriangleBudget {
    budget: u64,
}

impl MeshTriangleBudget {
    /// Budget from `config`.
    #[must_use]
    pub fn new(config: &ValidationConfig) -> Self {
        Self {
            budget: config.mesh_triangle_budget,
        }
    }
}

impl Validator for MeshTriangleBudget {
    fn name(&self) -> &str {
        "mesh_triangle_budget"
    }

    fn description(&self) -> &str {
        "meshes stay within the triangle budget"
    }

    fn validate(&self, _ctx: &ValidationContext<'_>, asset: &AssetEntry) -> Verdict {
        if asset.kind != AssetKind::Mesh {
            return Verdict::not_validated("not a mesh");
        }
        match asset.triangles {
            None => Verdict::not_validated("the mesh has no recorded triangle count"),
            Some(t) if t > self.budget => Verdict::fail(vec![format!(
                "the mesh has {t} triangles, over the budget of {}",
                self.budget
            )]),
            Some(_) => Verdict::pass(),
        }
    }
}

/// Errors of the registry.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidatorError {
    /// A validator with this name is already registered.
    #[error("a validator named `{0}` is already registered")]
    DuplicateName(String),
    /// No validator has this name.
    #[error("no validator named `{0}` is registered")]
    UnknownValidator(String),
    /// A name that is not lowercase ascii, digits and `_`.
    #[error("validator name `{0}` must be non-empty lowercase ascii letters, digits and `_`")]
    InvalidName(String),
    /// The report could not be written.
    #[error("report could not be written: {0}")]
    Write(String),
    /// The text is not a validation report ([`ValidationReport::from_ron`]).
    #[error("not a validation report: {0}")]
    Parse(String),
}

impl ValidatorError {
    /// Stable lowercase code for the agent protocol.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::DuplicateName(_) => "duplicate_validator",
            Self::UnknownValidator(_) => "unknown_validator",
            Self::InvalidName(_) => "invalid_validator_name",
            Self::Write(_) => "report_write",
            Self::Parse(_) => "report_invalid",
        }
    }
}

/// Counts of a [`ValidationReport`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportSummary {
    /// Rows that passed.
    pub passed: usize,
    /// Rows that failed.
    pub failed: usize,
    /// Rows that were not validated.
    pub not_validated: usize,
    /// Assets with at least one `Warning` finding.
    #[serde(default)]
    pub warnings: usize,
    /// Assets the scope asked for (named, gathered or unresolved), before exclusion and the cap.
    #[serde(default)]
    pub requested: usize,
    /// Assets that actually ran.
    #[serde(default)]
    pub checked: usize,
    /// Gathered assets dropped by the `exclude` globs.
    #[serde(default)]
    pub skipped_excluded: usize,
    /// Whether `max_assets` cut the list.
    #[serde(default)]
    pub cap_reached: bool,
}

/// The result of running validators over an [`AssetSet`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationReport {
    /// Format version of the serialized form ([`VALIDATION_REPORT_FORMAT_VERSION`]).
    pub format_version: u32,
    /// Counts per outcome.
    pub summary: ReportSummary,
    /// One row per (asset, validator), ordered by asset path then validator name.
    pub results: Vec<ValidationResult>,
}

impl ValidationReport {
    fn from_results(mut results: Vec<ValidationResult>) -> Self {
        results.sort_by(|a, b| (&a.asset, &a.validator).cmp(&(&b.asset, &b.validator)));
        let mut summary = ReportSummary::default();
        let mut warned: BTreeSet<&str> = BTreeSet::new();
        for r in &results {
            match r.outcome {
                Outcome::Pass => summary.passed += 1,
                Outcome::Fail => summary.failed += 1,
                Outcome::NotValidated => summary.not_validated += 1,
            }
            if r.findings.iter().any(|f| f.severity == Severity::Warning) {
                warned.insert(r.asset.as_str());
            }
        }
        summary.warnings = warned.len();
        Self {
            format_version: VALIDATION_REPORT_FORMAT_VERSION,
            summary,
            results,
        }
    }

    /// Whether no row failed.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.summary.failed == 0
    }

    /// Whether a CI run should exit nonzero: a row failed, or (`strict`) an asset has a warning.
    #[must_use]
    pub fn exit_failed(&self, strict: bool) -> bool {
        self.summary.failed > 0 || (strict && self.summary.warnings > 0)
    }

    /// Every finding with its asset and validator, in row order.
    pub fn all_findings(&self) -> impl Iterator<Item = (&ValidationResult, &Finding)> {
        self.results
            .iter()
            .flat_map(|r| r.findings.iter().map(move |f| (r, f)))
    }

    /// The failing rows.
    pub fn failures(&self) -> impl Iterator<Item = &ValidationResult> {
        self.results.iter().filter(|r| r.outcome == Outcome::Fail)
    }

    /// Pretty JSON.
    ///
    /// # Errors
    ///
    /// [`ValidatorError::Write`] if serialization fails.
    pub fn to_json(&self) -> Result<String, ValidatorError> {
        serde_json::to_string_pretty(self).map_err(|e| ValidatorError::Write(e.to_string()))
    }

    /// Pretty RON.
    ///
    /// # Errors
    ///
    /// [`ValidatorError::Write`] if serialization fails.
    pub fn to_ron(&self) -> Result<String, ValidatorError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| ValidatorError::Write(e.to_string()))
    }

    /// Parse a report back from RON.
    ///
    /// # Errors
    ///
    /// [`ValidatorError::Parse`] (`report_invalid`) carrying the parse message if the text is not
    /// a report (it was `Write`, `report_write`, before; audit-ext-b Low).
    pub fn from_ron(text: &str) -> Result<Self, ValidatorError> {
        ron::from_str(text).map_err(|e| ValidatorError::Parse(e.to_string()))
    }
}

/// Registered validators, run in registration order.
#[derive(Default)]
pub struct ValidatorRegistry {
    validators: Vec<Box<dyn Validator>>,
}

impl std::fmt::Debug for ValidatorRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValidatorRegistry")
            .field("validators", &self.names())
            .finish()
    }
}

impl ValidatorRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry holding the six built-ins configured by `config`.
    #[must_use]
    pub fn with_builtins(config: &ValidationConfig) -> Self {
        let mut registry = Self::new();
        let builtins: Vec<Box<dyn Validator>> = vec![
            Box::new(MissingReferences),
            Box::new(NamingRules::new(config)),
            Box::new(TextureSizeLimits::new(config)),
            Box::new(DuplicateIds),
            Box::new(OrphanedAssets),
            Box::new(MeshTriangleBudget::new(config)),
        ];
        for validator in builtins {
            // Built-in names are unique and valid, so this cannot fail.
            if let Err(e) = registry.register(validator) {
                tracing::error!(error = %e, "built-in validator rejected");
            }
        }
        registry
    }

    /// Add a validator.
    ///
    /// # Errors
    ///
    /// [`ValidatorError::InvalidName`] for a bad name, [`ValidatorError::DuplicateName`] if taken.
    pub fn register(&mut self, validator: Box<dyn Validator>) -> Result<(), ValidatorError> {
        let name = validator.name();
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(ValidatorError::InvalidName(name.to_owned()));
        }
        if self.validators.iter().any(|v| v.name() == name) {
            return Err(ValidatorError::DuplicateName(name.to_owned()));
        }
        self.validators.push(validator);
        Ok(())
    }

    /// Remove the validator called `name`.
    ///
    /// # Errors
    ///
    /// [`ValidatorError::UnknownValidator`] if there is none.
    pub fn unregister(&mut self, name: &str) -> Result<(), ValidatorError> {
        let before = self.validators.len();
        self.validators.retain(|v| v.name() != name);
        if self.validators.len() == before {
            return Err(ValidatorError::UnknownValidator(name.to_owned()));
        }
        Ok(())
    }

    /// Registered names in registration order.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.validators.iter().map(|v| v.name().to_owned()).collect()
    }

    /// `(name, description)` of every validator in registration order.
    #[must_use]
    pub fn describe(&self) -> Vec<(String, String)> {
        self.validators
            .iter()
            .map(|v| (v.name().to_owned(), v.description().to_owned()))
            .collect()
    }

    /// Run every validator over every asset of `set`.
    #[must_use]
    pub fn run(&self, set: &AssetSet) -> ValidationReport {
        self.run_with(set, &RunSettings::default())
    }

    /// Run only the validators called `names`.
    ///
    /// # Errors
    ///
    /// [`ValidatorError::UnknownValidator`] for a name that is not registered.
    pub fn run_only(&self, set: &AssetSet, names: &[&str]) -> Result<ValidationReport, ValidatorError> {
        let settings = RunSettings {
            only: Some(names.iter().map(|n| (*n).to_owned()).collect()),
            ..RunSettings::default()
        };
        self.run_checked(set, &settings)
    }

    /// Run with scope, cap, exclusions, severities and validator selection from `settings`. An unknown
    /// name in `settings.only` selects nothing (use [`ValidatorRegistry::run_checked`] to refuse it).
    #[must_use]
    pub fn run_with(&self, set: &AssetSet, settings: &RunSettings) -> ValidationReport {
        let only = settings.only.as_deref();
        self.execute(set, settings, |name| {
            only.is_none_or(|o| o.iter().any(|n| n == name))
        })
    }

    /// Like [`ValidatorRegistry::run_with`] but an unknown validator name in `settings.only` is an error.
    ///
    /// # Errors
    ///
    /// [`ValidatorError::UnknownValidator`] for a name that is not registered.
    pub fn run_checked(
        &self,
        set: &AssetSet,
        settings: &RunSettings,
    ) -> Result<ValidationReport, ValidatorError> {
        if let Some(only) = &settings.only {
            let known = self.names();
            if let Some(bad) = only.iter().find(|n| !known.contains(n)) {
                return Err(ValidatorError::UnknownValidator(bad.clone()));
            }
        }
        Ok(self.run_with(set, settings))
    }

    fn execute(
        &self,
        set: &AssetSet,
        settings: &RunSettings,
        keep: impl Fn(&str) -> bool,
    ) -> ValidationReport {
        let _span = tracing::info_span!(
            "validate_run",
            assets = set.assets.len(),
            validators = self.validators.len(),
            trigger = settings.trigger.label()
        )
        .entered();
        let ctx = ValidationContext::new(set);
        let gathered = gather(set, &ctx.graph, settings);
        let mut results = Vec::new();
        for name in &gathered.unresolved {
            results.push(ValidationResult {
                validator: "validation".to_owned(),
                asset: name.clone(),
                outcome: Outcome::Fail,
                messages: vec![format!("`{name}` is not an asset of the set")],
                findings: vec![Finding::error(
                    "validate_unknown_asset",
                    format!("`{name}` is not an asset of the set"),
                )],
            });
        }
        let entries: Vec<&AssetEntry> = gathered.paths.iter().filter_map(|p| set.get(p)).collect();
        for validator in self.validators.iter().filter(|v| keep(v.name())) {
            let _span = tracing::debug_span!("validate_validator", name = validator.name()).entered();
            for asset in &entries {
                let verdict = catch_unwind(AssertUnwindSafe(|| validator.validate(&ctx, asset)))
                    .unwrap_or_else(|_| {
                        Verdict::fail_with(vec![Finding::error(
                            "validate_validator_panicked",
                            format!("validator `{}` panicked on this asset", validator.name()),
                        )])
                    });
                results.push(finalize(
                    validator.name(),
                    &asset.path,
                    verdict,
                    settings.warnings_fail,
                ));
            }
        }
        let mut report = ValidationReport::from_results(results);
        report.summary.requested = gathered.requested;
        report.summary.checked = entries.len();
        report.summary.skipped_excluded = gathered.skipped_excluded;
        report.summary.cap_reached = gathered.cap_reached;
        report
    }
}

/// Apply the verdict contract (spec 32 § 4.3) and turn it into a report row.
fn finalize(validator: &str, asset: &str, verdict: Verdict, warnings_fail: bool) -> ValidationResult {
    let Verdict {
        mut outcome,
        mut messages,
        mut findings,
    } = verdict;
    for f in &mut findings {
        if f.code.is_empty() {
            f.code = validator.to_owned();
        }
    }
    match outcome {
        Outcome::NotValidated => findings.clear(),
        Outcome::Fail => {
            if !findings.iter().any(|f| f.severity == Severity::Error) {
                let text = "the validator failed without an error finding".to_owned();
                messages.push(text.clone());
                findings.push(Finding::error("validate_no_finding", text));
            }
        }
        Outcome::Pass => {
            let has_error = findings.iter().any(|f| f.severity == Severity::Error);
            let has_warning = findings.iter().any(|f| f.severity == Severity::Warning);
            if has_error || (warnings_fail && has_warning) {
                outcome = Outcome::Fail;
            }
        }
    }
    ValidationResult {
        validator: validator.to_owned(),
        asset: asset.to_owned(),
        outcome,
        messages,
        findings,
    }
}

/// Why a run started (spec 32 § 4.5). It labels the run; the caller chooses the scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// A person or script asked.
    #[default]
    Manual,
    /// After a save.
    Save,
    /// During a cook.
    Cook,
    /// A CI job.
    Ci,
    /// The agent protocol.
    Agent,
}

impl Trigger {
    /// Lowercase label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Save => "save",
            Self::Cook => "cook",
            Self::Ci => "ci",
            Self::Agent => "agent",
        }
    }
}

/// What a run covers (spec 32 § 4.3).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Paths or ids named directly (`exclude` does not apply to them).
    Assets(Vec<String>),
    /// Every asset under a folder.
    Folder {
        /// Folder path without a trailing `/`.
        path: String,
        /// Include subfolders.
        recursive: bool,
    },
    /// The named assets plus their transitive dependencies.
    WithDependencies(Vec<String>),
    /// Paths the caller says changed; assets that still reference a changed path that no longer
    /// exists are checked too.
    Changed(Vec<String>),
    /// Every asset.
    #[default]
    Project,
}

/// Run control (spec 32 § 4.3).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RunSettings {
    /// What to check.
    pub scope: Scope,
    /// Why the run started.
    pub trigger: Trigger,
    /// Stop gathering at this many assets (the sorted list is cut).
    pub max_assets: Option<usize>,
    /// Path globs applied to gathered (not directly named) assets.
    pub exclude: Vec<String>,
    /// A `Warning` finding also fails the asset.
    pub warnings_fail: bool,
    /// Run only these validators.
    pub only: Option<Vec<String>>,
}

struct Gathered {
    paths: Vec<String>,
    unresolved: Vec<String>,
    requested: usize,
    skipped_excluded: usize,
    cap_reached: bool,
}

/// Resolve a path or an id to an asset path.
fn resolve_name(set: &AssetSet, name: &str) -> Option<String> {
    set.get(name)
        .or_else(|| set.assets.iter().find(|a| a.id.as_deref() == Some(name)))
        .map(|a| a.path.clone())
}

fn gather(set: &AssetSet, graph: &DependencyGraph, settings: &RunSettings) -> Gathered {
    let excluded = |p: &str| settings.exclude.iter().any(|g| glob_match(g, p));
    // `named` bypass the exclusions, `found` honor them.
    let mut named: BTreeSet<String> = BTreeSet::new();
    let mut found: BTreeSet<String> = BTreeSet::new();
    let mut unresolved: BTreeSet<String> = BTreeSet::new();
    let resolve_all = |names: &[String], unresolved: &mut BTreeSet<String>, report_unknown: bool| {
        let mut ok = Vec::new();
        for n in names {
            match resolve_name(set, n) {
                Some(p) => ok.push(p),
                None if report_unknown => {
                    unresolved.insert(n.clone());
                }
                None => {}
            }
        }
        ok
    };
    match &settings.scope {
        Scope::Assets(names) => named.extend(resolve_all(names, &mut unresolved, true)),
        Scope::Folder { path, recursive } => {
            let prefix = format!("{}/", path.trim_end_matches('/'));
            for a in &set.assets {
                if let Some(rest) = a.path.strip_prefix(&prefix)
                    && (*recursive || !rest.contains('/'))
                {
                    found.insert(a.path.clone());
                }
            }
        }
        Scope::WithDependencies(names) => {
            let roots = resolve_all(names, &mut unresolved, true);
            for r in &roots {
                found.extend(graph.transitive_dependencies(r));
            }
            named.extend(roots);
        }
        Scope::Changed(paths) => {
            let present = resolve_all(paths, &mut unresolved, false);
            let gone: BTreeSet<&str> = paths
                .iter()
                .map(String::as_str)
                .filter(|p| resolve_name(set, p).is_none())
                .collect();
            for a in &set.assets {
                if a.references.iter().any(|r| gone.contains(r.as_str())) {
                    found.insert(a.path.clone());
                }
            }
            named.extend(present);
        }
        Scope::Project => found.extend(set.assets.iter().map(|a| a.path.clone())),
    }
    found.retain(|p| !named.contains(p));
    let requested = named.len() + found.len() + unresolved.len();
    let before = found.len();
    found.retain(|p| !excluded(p));
    let skipped_excluded = before - found.len();
    let mut paths: Vec<String> = named.into_iter().chain(found).collect();
    paths.sort();
    let mut cap_reached = false;
    if let Some(cap) = settings.max_assets
        && paths.len() > cap
    {
        paths.truncate(cap);
        cap_reached = true;
    }
    Gathered {
        paths,
        unresolved: unresolved.into_iter().collect(),
        requested,
        skipped_excluded,
        cap_reached,
    }
}

/// Own and rolled-up cooked size per asset of `set` (see [`DependencyGraph::size_rollup`]).
#[must_use]
pub fn size_map(set: &AssetSet) -> BTreeMap<String, SizeRollup> {
    DependencyGraph::build(set).size_rollup()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> AssetSet {
        let mut s = AssetSet::new();
        s.push(
            AssetEntry::new("scenes/main.scene", AssetKind::Scene)
                .with_id("s1")
                .with_references(["meshes/rock.mesh", "textures/gone.png"]),
        );
        s.push(
            AssetEntry::new("meshes/rock.mesh", AssetKind::Mesh)
                .with_id("m1")
                .with_triangles(1000)
                .with_references(["textures/rock.png"]),
        );
        s.push(
            AssetEntry::new("meshes/heavy.mesh", AssetKind::Mesh)
                .with_id("m2")
                .with_triangles(300_000),
        );
        s.push(
            AssetEntry::new("textures/rock.png", AssetKind::Texture)
                .with_id("t1")
                .with_texture_size(1024, 1024),
        );
        s.push(
            AssetEntry::new("textures/Huge Map.png", AssetKind::Texture)
                .with_id("t1")
                .with_texture_size(8192, 100),
        );
        s
    }

    fn run_one(v: impl Validator + 'static, set: &AssetSet) -> ValidationReport {
        let mut r = ValidatorRegistry::new();
        r.register(Box::new(v)).expect("registers");
        r.run(set)
    }

    fn row<'a>(rep: &'a ValidationReport, asset: &str) -> &'a ValidationResult {
        rep.results
            .iter()
            .find(|r| r.asset == asset)
            .expect("row present")
    }

    #[test]
    fn missing_references_fail_with_message() {
        let rep = run_one(MissingReferences, &set());
        let r = row(&rep, "scenes/main.scene");
        assert_eq!(r.outcome, Outcome::Fail);
        assert!(r.messages[0].contains("textures/gone.png"));
        assert_eq!(row(&rep, "meshes/rock.mesh").outcome, Outcome::Pass);
    }

    #[test]
    fn naming_rules_flag_bad_paths() {
        let cfg = ValidationConfig::default();
        let rep = run_one(NamingRules::new(&cfg), &set());
        assert_eq!(row(&rep, "textures/rock.png").outcome, Outcome::Pass);
        let bad = row(&rep, "textures/Huge Map.png");
        assert_eq!(bad.outcome, Outcome::Fail);
        assert!(bad.messages[0].contains("Huge Map.png"));
    }

    #[test]
    fn naming_rules_edge_cases() {
        let cfg = ValidationConfig {
            max_path_len: 16,
            ..ValidationConfig::default()
        };
        let mut s = AssetSet::new();
        for p in [
            "",
            "/abs/a.png",
            "a//b.png",
            "a/.hidden",
            "a\\b.png",
            "very/long/path.png",
            "ok/a_b-c.1.png",
            "a/",
        ] {
            s.push(AssetEntry::new(p, AssetKind::Other));
        }
        let rep = run_one(NamingRules::new(&cfg), &s);
        let failed: Vec<&str> = rep.failures().map(|r| r.asset.as_str()).collect();
        assert_eq!(
            failed,
            vec![
                "",
                "/abs/a.png",
                "a/",
                "a/.hidden",
                "a//b.png",
                "a\\b.png",
                "very/long/path.png"
            ]
        );
        assert_eq!(row(&rep, "ok/a_b-c.1.png").outcome, Outcome::Pass);
        assert!(row(&rep, "").messages[0].contains("empty"));
        assert!(row(&rep, "very/long/path.png").messages[0].contains("over the limit"));
    }

    /// audit-v035-cook L8: Windows device names and trailing-dot segments passed the portable
    /// names check, yet such files cannot be created under those names on Windows.
    #[test]
    fn naming_rules_refuse_windows_device_names_and_trailing_dots() {
        let cfg = ValidationConfig::default();
        let mut s = AssetSet::new();
        let bad = [
            "assets/con.png",
            "assets/nul.glb",
            "assets/aux",
            "assets/com1.ron",
            "assets/lpt9.tar.gz",
            "assets/foo./bar.png",
        ];
        let good = [
            "assets/console.png",
            "assets/com0.png",
            "assets/com10.png",
            "assets/nullable.ron",
            "assets/icon.png",
        ];
        for p in bad.iter().chain(&good) {
            s.push(AssetEntry::new(*p, AssetKind::Other));
        }
        let rep = run_one(NamingRules::new(&cfg), &s);
        for p in bad {
            assert_eq!(row(&rep, p).outcome, Outcome::Fail, "{p}");
        }
        for p in good {
            assert_eq!(row(&rep, p).outcome, Outcome::Pass, "{p}");
        }
        assert!(row(&rep, "assets/nul.glb").messages[0].contains("Windows device name"));
        assert!(row(&rep, "assets/foo./bar.png").messages[0].contains("ends with `.`"));
    }

    /// Cookers register sub-assets as `<source>#<label>` (terrain heightfields and tiles, meshlets,
    /// stream cells). The label is generated, not an authored file name, so `cook --ship` must not
    /// report `validate_name_invalid` for it (lane v05-tools-android-terrain-cook finding). A `#`
    /// in a path whose prefix is not a registered source is still a bad file name, an empty label
    /// is refused, and the source part keeps every rule.
    #[test]
    fn naming_rules_accept_generated_sub_asset_labels() {
        let cfg = ValidationConfig::default();
        let mut s = AssetSet::new();
        let good = [
            "assets/maps/ground/terrain.terrain.layers.ron",
            "assets/maps/ground/terrain.terrain.layers.ron#terrain/heightfield",
            "assets/maps/ground/terrain.terrain.layers.ron#terrain/tiles/albedo/0_1_2",
            "assets/props/rock.glb",
            "assets/props/rock.glb#mesh/Rock_LOD0",
        ];
        let bad = [
            "assets/tex/a#b.png",
            "assets/props/rock.glb#",
            "assets/Bad Dir/x.ron",
            "assets/Bad Dir/x.ron#cells/0_0",
        ];
        for p in good.iter().chain(&bad) {
            s.push(AssetEntry::new(*p, AssetKind::Other));
        }
        let rep = run_one(NamingRules::new(&cfg), &s);
        for p in good {
            assert_eq!(
                row(&rep, p).outcome,
                Outcome::Pass,
                "{p}: {:?}",
                row(&rep, p).messages
            );
        }
        for p in bad {
            assert_eq!(row(&rep, p).outcome, Outcome::Fail, "{p}");
        }
        assert!(row(&rep, "assets/tex/a#b.png").messages[0].contains("contains `#`"));
        assert!(row(&rep, "assets/props/rock.glb#").messages[0].contains("label"));
        assert!(row(&rep, "assets/Bad Dir/x.ron#cells/0_0").messages[0].contains("Bad Dir"));
    }

    fn finding_codes<'a>(rep: &'a ValidationReport, asset: &str) -> Vec<&'a str> {
        row(rep, asset).findings.iter().map(|f| f.code.as_str()).collect()
    }

    /// ADR-7961: the built-in rule allows uppercase ASCII in stems and directories. A space or a
    /// character outside ASCII letters, digits, `_`, `-` and `.` is still `validate_name_invalid`.
    #[test]
    fn naming_rules_allow_uppercase_ascii_and_still_reject_space_and_non_ascii() {
        let mut s = AssetSet::new();
        let good = [
            "assets/SM_BloodPool_B.gltf",
            "assets/Textures/T_Rock_D.png",
            "assets/props/console.PNG",
        ];
        let bad = ["assets/Bad Name.png", "assets/caf\u{e9}.png"];
        for p in good.iter().chain(&bad) {
            s.push(AssetEntry::new(*p, AssetKind::Other));
        }
        let rep = run_one(NamingRules::new(&ValidationConfig::default()), &s);
        for p in good {
            assert_eq!(
                row(&rep, p).outcome,
                Outcome::Pass,
                "{p}: {:?}",
                row(&rep, p).messages
            );
            assert!(finding_codes(&rep, p).is_empty(), "{p}");
        }
        for p in bad {
            assert_eq!(row(&rep, p).outcome, Outcome::Fail, "{p}");
            assert_eq!(finding_codes(&rep, p), ["validate_name_invalid"], "{p}");
        }
        assert!(row(&rep, "assets/Bad Name.png").messages[0].contains("Bad Name.png"));
        assert!(row(&rep, "assets/caf\u{e9}.png").messages[0].contains("caf\u{e9}.png"));
    }

    /// `CON` and `Aux` are the same Windows devices as `con` and `aux`. The match is
    /// case-insensitive, so an uppercase stem is not a way around the device-name rule.
    #[test]
    fn naming_rules_refuse_windows_device_names_in_any_ascii_case() {
        let mut s = AssetSet::new();
        let bad = [
            "assets/CON.gltf",
            "assets/Aux.png",
            "assets/Com1.ron",
            "assets/lPt9.tar.gz",
        ];
        let good = ["assets/Console.png", "assets/Com0.png", "assets/Com10.png"];
        for p in bad.iter().chain(&good) {
            s.push(AssetEntry::new(*p, AssetKind::Other));
        }
        let rep = run_one(NamingRules::new(&ValidationConfig::default()), &s);
        for p in bad {
            assert_eq!(row(&rep, p).outcome, Outcome::Fail, "{p}");
            assert_eq!(finding_codes(&rep, p), ["validate_name_invalid"], "{p}");
            assert!(
                row(&rep, p).messages[0].contains("Windows device name"),
                "{p}: {}",
                row(&rep, p).messages[0]
            );
        }
        for p in good {
            assert_eq!(
                row(&rep, p).outcome,
                Outcome::Pass,
                "{p}: {:?}",
                row(&rep, p).messages
            );
        }
    }

    /// Two file paths that differ only by ASCII letter case cannot both exist on Windows or on
    /// the default macOS volume. The code is `validate_name_case_collision`, not
    /// `validate_name_invalid`. A generated `#` label is not part of the file path.
    #[test]
    fn naming_rules_flag_paths_that_differ_only_by_case() {
        let mut s = AssetSet::new();
        for p in [
            "assets/Foo.gltf",
            "assets/foo.gltf",
            "assets/Bar.gltf",
            "assets/Dir/a.png",
            "assets/dir/a.png",
            "assets/A.png",
            "assets/a.png",
            "assets/A.PNG",
            "assets/rock.glb",
            "assets/rock.glb#mesh/Rock_LOD0",
            "assets/rock.glb#mesh/rock_lod0",
            "assets/Only.gltf",
            "assets/Only.gltf",
        ] {
            s.push(AssetEntry::new(p, AssetKind::Other));
        }
        let rep = run_one(NamingRules::new(&ValidationConfig::default()), &s);
        for p in [
            "assets/Foo.gltf",
            "assets/foo.gltf",
            "assets/Dir/a.png",
            "assets/dir/a.png",
        ] {
            assert_eq!(row(&rep, p).outcome, Outcome::Fail, "{p}");
            assert_eq!(finding_codes(&rep, p), ["validate_name_case_collision"], "{p}");
        }
        assert!(row(&rep, "assets/Foo.gltf").messages[0].contains("assets/foo.gltf"));
        assert!(row(&rep, "assets/foo.gltf").messages[0].contains("assets/Foo.gltf"));
        assert!(row(&rep, "assets/Dir/a.png").messages[0].contains("assets/dir/a.png"));
        for p in ["assets/A.png", "assets/a.png", "assets/A.PNG"] {
            assert_eq!(finding_codes(&rep, p), ["validate_name_case_collision"], "{p}");
            let text = &row(&rep, p).messages[0];
            for other in ["assets/A.png", "assets/a.png", "assets/A.PNG"] {
                if other != p {
                    assert!(text.contains(other), "{p} should name {other}: {text}");
                }
            }
        }
        for p in [
            "assets/Bar.gltf",
            "assets/rock.glb",
            "assets/rock.glb#mesh/Rock_LOD0",
            "assets/rock.glb#mesh/rock_lod0",
            "assets/Only.gltf",
        ] {
            assert_eq!(
                row(&rep, p).outcome,
                Outcome::Pass,
                "{p}: {:?}",
                row(&rep, p).messages
            );
        }
    }

    /// A space stays `validate_name_invalid` even when the same stem also collides by case.
    #[test]
    fn naming_rules_report_a_case_collision_beside_another_naming_error() {
        let mut s = AssetSet::new();
        s.push(AssetEntry::new("assets/Bad Name.png", AssetKind::Other));
        s.push(AssetEntry::new("assets/bad name.png", AssetKind::Other));
        let rep = run_one(NamingRules::new(&ValidationConfig::default()), &s);
        for p in ["assets/Bad Name.png", "assets/bad name.png"] {
            assert_eq!(
                finding_codes(&rep, p),
                ["validate_name_invalid", "validate_name_case_collision"],
                "{p}"
            );
        }
    }

    #[test]
    fn texture_limits() {
        let cfg = ValidationConfig::default();
        let rep = run_one(TextureSizeLimits::new(&cfg), &set());
        assert_eq!(row(&rep, "textures/rock.png").outcome, Outcome::Pass);
        assert_eq!(row(&rep, "textures/Huge Map.png").outcome, Outcome::Fail);
        assert_eq!(row(&rep, "meshes/rock.mesh").outcome, Outcome::NotValidated);
    }

    #[test]
    fn texture_limit_edge_cases() {
        let cfg = ValidationConfig {
            max_texture_dimension: 2048,
            texture_power_of_two: true,
            ..ValidationConfig::default()
        };
        let mut s = AssetSet::new();
        s.push(AssetEntry::new("exact.png", AssetKind::Texture).with_texture_size(2048, 2048));
        s.push(AssetEntry::new("npot.png", AssetKind::Texture).with_texture_size(1000, 512));
        s.push(AssetEntry::new("zero.png", AssetKind::Texture).with_texture_size(0, 64));
        s.push(AssetEntry::new("nosize.png", AssetKind::Texture));
        let rep = run_one(TextureSizeLimits::new(&cfg), &s);
        assert_eq!(row(&rep, "exact.png").outcome, Outcome::Pass);
        assert_eq!(row(&rep, "npot.png").outcome, Outcome::Fail);
        assert!(row(&rep, "npot.png").messages[0].contains("powers of two"));
        assert_eq!(row(&rep, "zero.png").outcome, Outcome::Fail);
        assert_eq!(
            row(&rep, "zero.png").messages.len(),
            1,
            "zero is not also reported as npot"
        );
        assert_eq!(row(&rep, "nosize.png").outcome, Outcome::NotValidated);
    }

    #[test]
    fn duplicate_ids_name_the_other_owner() {
        let rep = run_one(DuplicateIds, &set());
        let a = row(&rep, "textures/rock.png");
        assert_eq!(a.outcome, Outcome::Fail);
        assert!(a.messages[0].contains("textures/Huge Map.png"));
        assert_eq!(row(&rep, "meshes/rock.mesh").outcome, Outcome::Pass);
    }

    #[test]
    fn a_widely_shared_id_lists_a_few_owners_and_counts_the_rest() {
        // 2000 assets on one id: each message used to name all 1999 others (ADR-1660).
        let mut s = AssetSet::new();
        for k in 0..2000 {
            s.push(AssetEntry::new(format!("t/{k:04}.png"), AssetKind::Texture).with_id("same"));
        }
        let rep = run_one(DuplicateIds, &s);
        let first = row(&rep, "t/0000.png");
        assert_eq!(first.outcome, Outcome::Fail);
        let message = &first.messages[0];
        assert!(
            message.contains("`t/0001.png`") && message.contains("`t/0008.png`"),
            "{message}"
        );
        assert!(!message.contains("t/0009.png"), "{message}");
        assert!(message.ends_with(" and 1991 more"), "{message}");
        assert!(rep.results.iter().all(|r| r.outcome == Outcome::Fail));
        // Few owners: the message is unchanged (every other owner, no count).
        let rep = run_one(DuplicateIds, &set());
        assert!(!row(&rep, "textures/rock.png").messages[0].contains("more"));
    }

    #[test]
    fn duplicate_paths_and_missing_ids() {
        let mut s = AssetSet::new();
        s.push(AssetEntry::new("a.png", AssetKind::Texture).with_id("x"));
        s.push(AssetEntry::new("a.png", AssetKind::Texture).with_id("y"));
        s.push(AssetEntry::new("noid.png", AssetKind::Texture));
        let rep = run_one(DuplicateIds, &s);
        assert!(
            rep.results
                .iter()
                .filter(|r| r.asset == "a.png")
                .all(|r| r.outcome == Outcome::Fail)
        );
        assert_eq!(row(&rep, "noid.png").outcome, Outcome::NotValidated);
    }

    #[test]
    fn orphans_follow_references_from_scenes() {
        let rep = run_one(OrphanedAssets, &set());
        assert_eq!(row(&rep, "scenes/main.scene").outcome, Outcome::Pass);
        assert_eq!(row(&rep, "textures/rock.png").outcome, Outcome::Pass);
        let orphan = row(&rep, "meshes/heavy.mesh");
        assert_eq!(orphan.outcome, Outcome::Fail);
        assert!(orphan.messages[0].contains("nothing references it"));
    }

    #[test]
    fn orphan_message_names_unreachable_referrers() {
        let mut s = AssetSet::new();
        s.push(AssetEntry::new("scene.scene", AssetKind::Scene));
        s.push(AssetEntry::new("a.mat", AssetKind::Material).with_references(["b.png"]));
        s.push(AssetEntry::new("b.png", AssetKind::Texture));
        let rep = run_one(OrphanedAssets, &s);
        assert!(row(&rep, "b.png").messages[0].contains("a.mat"));
    }

    #[test]
    fn orphan_messages_name_roots_that_are_not_in_the_set() {
        // `roots: ["scenes/mian.scene"]` made every asset an orphan with no hint why (ADR-1660).
        let mut s = AssetSet::new();
        s.push(AssetEntry::new("scenes/main.scene", AssetKind::Scene).with_references(["a.png"]));
        s.push(AssetEntry::new("a.png", AssetKind::Texture));
        s.roots = vec!["scenes/mian.scene".into()];
        let rep = run_one(OrphanedAssets, &s);
        let a = row(&rep, "a.png");
        assert_eq!(a.outcome, Outcome::Fail);
        assert!(
            a.messages[0].contains("roots not in the set: `scenes/mian.scene`"),
            "{a:?}"
        );
        // A correct root: no note.
        s.roots = vec!["scenes/main.scene".into()];
        s.push(AssetEntry::new("lonely.png", AssetKind::Texture));
        let rep = run_one(OrphanedAssets, &s);
        assert_eq!(row(&rep, "a.png").outcome, Outcome::Pass);
        assert!(!row(&rep, "lonely.png").messages[0].contains("roots not in the set"));
    }

    #[test]
    fn orphans_without_roots_are_not_validated_and_explicit_roots_win() {
        let mut s = AssetSet::new();
        s.push(AssetEntry::new("a.png", AssetKind::Texture));
        let rep = run_one(OrphanedAssets, &s);
        assert_eq!(row(&rep, "a.png").outcome, Outcome::NotValidated);
        s.roots = vec!["a.png".into()];
        assert_eq!(row(&run_one(OrphanedAssets, &s), "a.png").outcome, Outcome::Pass);
    }

    #[test]
    fn triangle_budget() {
        let cfg = ValidationConfig::default();
        let rep = run_one(MeshTriangleBudget::new(&cfg), &set());
        assert_eq!(row(&rep, "meshes/rock.mesh").outcome, Outcome::Pass);
        assert_eq!(row(&rep, "meshes/heavy.mesh").outcome, Outcome::Fail);
        assert_eq!(row(&rep, "textures/rock.png").outcome, Outcome::NotValidated);
        let mut s = AssetSet::new();
        s.push(AssetEntry::new("edge.mesh", AssetKind::Mesh).with_triangles(200_000));
        s.push(AssetEntry::new("none.mesh", AssetKind::Mesh));
        let rep = run_one(MeshTriangleBudget::new(&cfg), &s);
        assert_eq!(
            row(&rep, "edge.mesh").outcome,
            Outcome::Pass,
            "the budget itself is allowed"
        );
        assert_eq!(row(&rep, "none.mesh").outcome, Outcome::NotValidated);
    }

    fn twin(path: &str, kind: AssetKind, base: &str) -> AssetEntry {
        let mut entry = AssetEntry::new(path, kind);
        entry.tags.insert("localized_of".into(), base.into());
        entry
    }

    fn twin_outcome(set: &AssetSet, path: &str) -> Verdict {
        let ctx = ValidationContext::new(set);
        let asset = set.get(path).expect("the asset is in the set");
        LocalizedTwin.validate(&ctx, asset)
    }

    #[test]
    fn localized_twin_judges_type_and_base() {
        let mut set = AssetSet::new();
        set.push(AssetEntry::new("ui/sign.png", AssetKind::Texture));
        set.push(twin(
            "localized/fr/ui/sign.png",
            AssetKind::Texture,
            "ui/sign.png",
        ));
        set.push(twin("localized/de/ui/sign.png", AssetKind::Mesh, "ui/sign.png"));
        set.push(twin(
            "localized/fr/ui/gone.png",
            AssetKind::Texture,
            "ui/gone.png",
        ));
        assert_eq!(twin_outcome(&set, "ui/sign.png").outcome, Outcome::NotValidated);
        assert_eq!(
            twin_outcome(&set, "localized/fr/ui/sign.png").outcome,
            Outcome::Pass
        );
        let wrong = twin_outcome(&set, "localized/de/ui/sign.png");
        assert_eq!(wrong.outcome, Outcome::Fail);
        assert_eq!(wrong.findings[0].code, "validate_localized_type_mismatch");
        assert_eq!(wrong.findings[0].severity, Severity::Error);
        // No base warns: the variant still loads for its own locale.
        let orphan = twin_outcome(&set, "localized/fr/ui/gone.png");
        assert_eq!(orphan.outcome, Outcome::Pass);
        assert_eq!(orphan.findings[0].code, "validate_localized_no_base");
        assert_eq!(orphan.findings[0].severity, Severity::Warning);
    }

    #[test]
    fn localized_twin_compares_the_cooked_type_when_the_kinds_agree() {
        let mut base = AssetEntry::new("data/a.bin", AssetKind::Other);
        base.tags.insert(COOKED_TYPE_TAG.into(), "x::Table".into());
        let mut variant = twin("localized/fr/data/a.bin", AssetKind::Other, "data/a.bin");
        variant.tags.insert(COOKED_TYPE_TAG.into(), "x::Curve".into());
        let mut set = AssetSet::new();
        set.push(base);
        set.push(variant);
        assert_eq!(
            twin_outcome(&set, "localized/fr/data/a.bin").outcome,
            Outcome::Fail
        );
    }

    #[test]
    fn registry_builtins_and_ordering() {
        let reg = ValidatorRegistry::with_builtins(&ValidationConfig::default());
        assert_eq!(
            reg.names(),
            vec![
                "missing_references",
                "naming_rules",
                "texture_size_limits",
                "duplicate_ids",
                "orphaned_assets",
                "mesh_triangle_budget"
            ]
        );
        let rep = reg.run(&set());
        assert_eq!(rep.results.len(), 5 * 6);
        assert!(!rep.is_ok());
        assert_eq!(
            rep.summary.passed + rep.summary.failed + rep.summary.not_validated,
            rep.results.len()
        );
        let keys: Vec<_> = rep
            .results
            .iter()
            .map(|r| (r.asset.clone(), r.validator.clone()))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        assert_eq!(rep, reg.run(&set()), "deterministic");
    }

    #[test]
    fn registry_errors() {
        let mut reg = ValidatorRegistry::with_builtins(&ValidationConfig::default());
        assert_eq!(
            reg.register(Box::new(DuplicateIds)).expect_err("duplicate"),
            ValidatorError::DuplicateName("duplicate_ids".into())
        );
        struct Bad;
        impl Validator for Bad {
            fn name(&self) -> &str {
                "Bad Name"
            }
            fn description(&self) -> &str {
                ""
            }
            fn validate(&self, _: &ValidationContext<'_>, _: &AssetEntry) -> Verdict {
                Verdict::pass()
            }
        }
        let e = reg.register(Box::new(Bad)).expect_err("invalid");
        assert_eq!(e.code(), "invalid_validator_name");
        assert_eq!(
            reg.unregister("nope").expect_err("unknown").code(),
            "unknown_validator"
        );
        reg.unregister("naming_rules").expect("removes");
        assert!(!reg.names().contains(&"naming_rules".to_owned()));
        assert_eq!(
            reg.run_only(&set(), &["missing_references", "zzz"])
                .expect_err("unknown")
                .code(),
            "unknown_validator"
        );
    }

    #[test]
    fn custom_validator_and_run_only() {
        struct NoMeshes;
        impl Validator for NoMeshes {
            fn name(&self) -> &str {
                "no_meshes"
            }
            fn description(&self) -> &str {
                "forbids meshes"
            }
            fn validate(&self, _: &ValidationContext<'_>, a: &AssetEntry) -> Verdict {
                if a.kind == AssetKind::Mesh {
                    Verdict::fail(vec!["meshes are forbidden".into()])
                } else {
                    Verdict::pass()
                }
            }
        }
        let mut reg = ValidatorRegistry::with_builtins(&ValidationConfig::default());
        reg.register(Box::new(NoMeshes)).expect("registers");
        let rep = reg.run_only(&set(), &["no_meshes"]).expect("runs");
        assert_eq!(rep.results.len(), 5);
        assert_eq!(rep.summary.failed, 2);
    }

    #[test]
    fn empty_set_gives_empty_ok_report() {
        let rep = ValidatorRegistry::with_builtins(&ValidationConfig::default()).run(&AssetSet::new());
        assert!(rep.results.is_empty());
        assert!(rep.is_ok());
    }

    #[test]
    fn report_serializes_to_json_and_ron() {
        let rep = ValidatorRegistry::with_builtins(&ValidationConfig::default()).run(&set());
        let json: serde_json::Value = serde_json::from_str(&rep.to_json().expect("json")).expect("parses");
        assert_eq!(json["format_version"], 1);
        assert_eq!(json["summary"]["failed"], rep.summary.failed);
        assert_eq!(json["results"][0]["outcome"], rep.results[0].outcome.label());
        let ron_text = rep.to_ron().expect("ron");
        assert_eq!(ValidationReport::from_ron(&ron_text).expect("round trip"), rep);
        // A parse failure has its own code; `report_write` is for a failed write (audit-ext-b).
        let bad = ValidationReport::from_ron("(").expect_err("bad");
        assert_eq!(bad.code(), "report_invalid");
        assert!(matches!(bad, ValidatorError::Parse(_)), "{bad:?}");
    }

    #[test]
    fn config_defaults_from_partial_ron() {
        let cfg: ValidationConfig = ron::from_str("(mesh_triangle_budget: 10)").expect("parses");
        assert_eq!(cfg.mesh_triangle_budget, 10);
        assert_eq!(cfg.max_texture_dimension, 4096);
    }

    #[test]
    fn size_map_matches_graph_rollup() {
        let mut s = AssetSet::new();
        s.push(
            AssetEntry::new("a", AssetKind::Other)
                .with_references(["b"])
                .with_cooked_bytes(3),
        );
        s.push(AssetEntry::new("b", AssetKind::Other).with_cooked_bytes(4));
        let m = size_map(&s);
        assert_eq!(m["a"].total_bytes, 7);
        assert_eq!(m["b"].total_bytes, 4);
    }
}
