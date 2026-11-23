// SPDX-License-Identifier: MPL-2.0
//! The project's provenance policy, `config/provenance.ron` (`docs/specs/12-assets.md` § 1a,
//! `docs/specs/23-agent-native.md` § 5, ADR-2428, `docs/formats/provenance-ron.md`).
//!
//! The policy names the provenance fields each asset kind must carry, the tools, origins and
//! licences a project accepts, the review states that count as reviewed, and the project's own
//! extension fields (DO27 declares `foreign_game_file` and `third_party_mod_file`). It is checked
//! against every source's `.meta` by `regia assets validate` (validator `provenance_policy`) and by
//! `regia cook --quality final` and `regia package`, which refuse an asset that breaks it with the
//! code `provenance_policy`.
//!
//! A project without the file has no policy: every check here is skipped and the cook behaves as
//! before the policy existed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use regia_asset::{MetaSidecar, Provenance, ProvenanceTool, ReviewerState};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::asset_graph::AssetEntry;
use crate::lod_classes::glob_match;
use crate::validators::{Finding, Severity, ValidationContext, Validator, Verdict};

/// Current `config/provenance.ron` format version.
pub const PROVENANCE_POLICY_FORMAT_VERSION: u32 = 1;

/// Project-relative location of the policy file.
pub const PROVENANCE_POLICY_PATH: &str = "config/provenance.ron";

/// The stable code of every policy refusal and finding.
pub const PROVENANCE_POLICY_CODE: &str = "provenance_policy";

/// The extension field that carries the licence when `licence_field` is not set.
pub const DEFAULT_LICENCE_FIELD: &str = "licence";

/// The built-in provenance block fields a `required` list may name.
pub const BUILTIN_FIELDS: &[&str] = &[
    "origin",
    "tool",
    "generator",
    "seed",
    "client",
    "session",
    "model",
    "created_utc",
    "reviewer",
];

/// Longest policy file read (a policy is a few hundred lines at most).
pub const MAX_POLICY_BYTES: usize = 1 << 20;

/// One project-defined provenance field (an `extensions` key of the `.meta` provenance block).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyField {
    /// The key (lowercase ascii, digits and `_`).
    pub name: String,
    /// The values allowed; empty allows any non-empty value.
    #[serde(default)]
    pub values: Vec<String>,
    /// What the field records (shown in the editor and the agent answer).
    #[serde(default)]
    pub description: String,
}

/// The rules for one asset kind (a `.meta` importer id such as `gltf`, `png` or `wav`; `*` is
/// every kind).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KindRule {
    /// Fields the provenance block must carry: built-in names ([`BUILTIN_FIELDS`]) or declared
    /// extension fields.
    #[serde(default)]
    pub required: Vec<String>,
}

/// A review state as the policy names it (`named` stands for any named reviewer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewKind {
    /// Waiting for review.
    Pending,
    /// Reviewed by the project owner.
    Owner,
    /// Reviewed by a named reviewer.
    Named,
}

impl ReviewKind {
    /// The kind of a `.meta` review state.
    #[must_use]
    pub fn of(state: &ReviewerState) -> Self {
        match state {
            ReviewerState::Pending => Self::Pending,
            ReviewerState::Owner => Self::Owner,
            ReviewerState::Named(_) => Self::Named,
        }
    }

    /// Lowercase label (`pending`, `owner`, `named`).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Owner => "owner",
            Self::Named => "named",
        }
    }
}

/// Which assets need a review before shipping, and which states count as reviewed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewRule {
    /// Tools whose assets must be reviewed (for example `[agent, generator]`); empty: none.
    #[serde(default)]
    pub required_for: Vec<ProvenanceTool>,
    /// States that count as reviewed; empty means `[owner, named]`.
    #[serde(default)]
    pub accepted: Vec<ReviewKind>,
}

impl ReviewRule {
    /// The accepted states with the default applied.
    #[must_use]
    pub fn accepted_states(&self) -> Vec<ReviewKind> {
        if self.accepted.is_empty() {
            vec![ReviewKind::Owner, ReviewKind::Named]
        } else {
            self.accepted.clone()
        }
    }
}

/// `config/provenance.ron` v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvenancePolicy {
    /// Format version ([`PROVENANCE_POLICY_FORMAT_VERSION`]).
    #[serde(default = "default_version")]
    pub format_version: u32,
    /// The project's own provenance fields (the `extensions` keys it records).
    #[serde(default)]
    pub fields: Vec<PolicyField>,
    /// Whether an `extensions` key the policy does not declare is a warning.
    #[serde(default)]
    pub warn_undeclared: bool,
    /// Required fields per asset kind (importer id; `*` applies to every kind).
    #[serde(default)]
    pub kinds: BTreeMap<String, KindRule>,
    /// Tools accepted (`hand`, `agent`, `generator`, `import`, `unknown`); empty accepts every tool.
    #[serde(default)]
    pub tools: Vec<ProvenanceTool>,
    /// Origins accepted, as globs over the `origin` string; empty accepts any origin.
    #[serde(default)]
    pub origins: Vec<String>,
    /// The extension field that holds the licence (default `licence`).
    #[serde(default)]
    pub licence_field: Option<String>,
    /// Licences accepted (exact, case-sensitive); empty accepts any licence.
    #[serde(default)]
    pub licences: Vec<String>,
    /// The review rule.
    #[serde(default)]
    pub review: ReviewRule,
    /// Source path globs (project-relative, `assets/...`) the policy does not apply to.
    #[serde(default)]
    pub exclude: Vec<String>,
}

fn default_version() -> u32 {
    PROVENANCE_POLICY_FORMAT_VERSION
}

impl Default for ProvenancePolicy {
    fn default() -> Self {
        Self {
            format_version: PROVENANCE_POLICY_FORMAT_VERSION,
            fields: Vec::new(),
            warn_undeclared: false,
            kinds: BTreeMap::new(),
            tools: Vec::new(),
            origins: Vec::new(),
            licence_field: None,
            licences: Vec::new(),
            review: ReviewRule::default(),
            exclude: Vec::new(),
        }
    }
}

/// Errors reading `config/provenance.ron`.
#[derive(Debug, Error)]
pub enum ProvenancePolicyError {
    /// The RON did not parse, or has an unknown field or variant.
    #[error("config/provenance.ron is invalid: {0}")]
    Parse(String),
    /// Written by a newer format.
    #[error("config/provenance.ron format_version {found} is newer than the supported {supported}")]
    Version {
        /// Version in the file.
        found: u32,
        /// Newest version read.
        supported: u32,
    },
    /// A rule that cannot work (an unknown required field, a repeated field, a bad name).
    #[error("config/provenance.ron: {0}")]
    Bad(String),
    /// The file could not be read.
    #[error("cannot read config/provenance.ron: {0}")]
    Io(String),
}

impl ProvenancePolicyError {
    /// The stable code (`invalid_config` for a bad file, `io` when it cannot be read).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "io",
            _ => "invalid_config",
        }
    }
}

fn is_key(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

impl ProvenancePolicy {
    /// Parse and check a policy.
    ///
    /// # Errors
    ///
    /// [`ProvenancePolicyError`] for bad RON (unknown fields included), a newer version, or a rule
    /// that names an undeclared field.
    pub fn from_ron(text: &str) -> Result<Self, ProvenancePolicyError> {
        if text.len() > MAX_POLICY_BYTES {
            return Err(ProvenancePolicyError::Bad(format!(
                "the file is larger than {MAX_POLICY_BYTES} bytes"
            )));
        }
        // `IMPLICIT_SOME`: optional fields are written bare (`licence_field: "licence"`).
        let opts = ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME);
        let policy: Self = opts
            .from_str(text)
            .map_err(|e| ProvenancePolicyError::Parse(e.to_string()))?;
        if policy.format_version > PROVENANCE_POLICY_FORMAT_VERSION {
            return Err(ProvenancePolicyError::Version {
                found: policy.format_version,
                supported: PROVENANCE_POLICY_FORMAT_VERSION,
            });
        }
        policy.check()?;
        Ok(policy)
    }

    fn check(&self) -> Result<(), ProvenancePolicyError> {
        let mut declared = BTreeSet::new();
        for field in &self.fields {
            if !is_key(&field.name) {
                return Err(ProvenancePolicyError::Bad(format!(
                    "field name `{}` must be lowercase ascii, digits and `_` (1 to 64 bytes)",
                    field.name
                )));
            }
            if BUILTIN_FIELDS.contains(&field.name.as_str()) {
                return Err(ProvenancePolicyError::Bad(format!(
                    "field `{}` is a built-in provenance field; declare only extension fields",
                    field.name
                )));
            }
            if !declared.insert(field.name.as_str()) {
                return Err(ProvenancePolicyError::Bad(format!(
                    "field `{}` is declared twice",
                    field.name
                )));
            }
        }
        let licence = self.licence_key();
        for (kind, rule) in &self.kinds {
            if kind != "*" && !is_key(kind) {
                return Err(ProvenancePolicyError::Bad(format!(
                    "kind `{kind}` must be an importer id (lowercase ascii, digits and `_`) or `*`"
                )));
            }
            for name in &rule.required {
                let known = BUILTIN_FIELDS.contains(&name.as_str())
                    || declared.contains(name.as_str())
                    || name == licence;
                if !known {
                    return Err(ProvenancePolicyError::Bad(format!(
                        "kind `{kind}` requires `{name}`, which is neither a built-in field nor declared in `fields`"
                    )));
                }
            }
        }
        if !is_key(licence) {
            return Err(ProvenancePolicyError::Bad(format!(
                "licence_field `{licence}` must be lowercase ascii, digits and `_`"
            )));
        }
        Ok(())
    }

    /// Serialize as pretty RON.
    ///
    /// # Errors
    ///
    /// A serialization failure (never for a policy that parsed).
    pub fn to_ron(&self) -> Result<String, ProvenancePolicyError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| ProvenancePolicyError::Parse(e.to_string()))
    }

    /// The extension key holding the licence.
    #[must_use]
    pub fn licence_key(&self) -> &str {
        self.licence_field.as_deref().unwrap_or(DEFAULT_LICENCE_FIELD)
    }

    /// The fields `kind` must carry: the `*` list, then the kind's own, without repeats.
    #[must_use]
    pub fn required_for(&self, kind: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for rule in [self.kinds.get("*"), self.kinds.get(kind)].into_iter().flatten() {
            for name in &rule.required {
                if !out.contains(name) {
                    out.push(name.clone());
                }
            }
        }
        out
    }

    /// Whether `source` (project-relative, `assets/...`) is excluded.
    #[must_use]
    pub fn excludes(&self, source: &str) -> bool {
        self.exclude.iter().any(|g| glob_match(g, source))
    }

    /// Whether `provenance` (absent: an empty block) needs a review the policy does not see.
    #[must_use]
    pub fn needs_review(&self, provenance: Option<&Provenance>) -> bool {
        let empty = Provenance::default();
        let p = provenance.unwrap_or(&empty);
        self.review.required_for.contains(&p.tool)
            && !self
                .review
                .accepted_states()
                .contains(&ReviewKind::of(&p.reviewer))
    }

    /// The findings for one source (`source` project-relative) and its `.meta`.
    #[must_use]
    pub fn check_meta(&self, source: &str, meta: &MetaSidecar) -> Vec<PolicyFinding> {
        if self.excludes(source) {
            return Vec::new();
        }
        let empty = Provenance::default();
        let p = meta.provenance.as_ref().unwrap_or(&empty);
        let mut out = Vec::new();
        let mut push = |rule: &'static str, severity: Severity, text: String| {
            out.push(PolicyFinding {
                source: source.to_owned(),
                rule,
                severity,
                text,
            });
        };
        let licence_key = self.licence_key();
        for name in self.required_for(&meta.importer) {
            if !field_present(p, &name) {
                let why = if meta.provenance.is_none() {
                    " (the .meta has no provenance block)"
                } else {
                    ""
                };
                push(
                    "missing_field",
                    Severity::Error,
                    format!("`{name}` is required for {} assets{why}", meta.importer),
                );
            }
        }
        if !self.tools.is_empty() && !self.tools.contains(&p.tool) {
            push(
                "tool_not_allowed",
                Severity::Error,
                format!("tool `{}` is not allowed", tool_label(&p.tool)),
            );
        }
        if !self.origins.is_empty()
            && let Some(origin) = p.origin.as_deref().filter(|o| !o.is_empty())
            && !self.origins.iter().any(|g| glob_match(g, origin))
        {
            push(
                "origin_not_allowed",
                Severity::Error,
                format!("origin `{origin}` matches none of the allowed origins"),
            );
        }
        if !self.licences.is_empty()
            && let Some(licence) = p.extensions.get(licence_key).filter(|l| !l.is_empty())
            && !self.licences.contains(licence)
        {
            push(
                "licence_not_allowed",
                Severity::Error,
                format!("licence `{licence}` is not allowed"),
            );
        }
        for field in &self.fields {
            if field.values.is_empty() {
                continue;
            }
            if let Some(value) = p.extensions.get(&field.name)
                && !field.values.contains(value)
            {
                push(
                    "value_not_allowed",
                    Severity::Error,
                    format!(
                        "`{}` is `{value}`; allowed: {}",
                        field.name,
                        field.values.join(", ")
                    ),
                );
            }
        }
        if self.needs_review(meta.provenance.as_ref()) {
            push(
                "unreviewed",
                Severity::Error,
                format!(
                    "made by `{}` and its review state is `{}`; accepted: {}",
                    tool_label(&p.tool),
                    ReviewKind::of(&p.reviewer).label(),
                    self.review
                        .accepted_states()
                        .iter()
                        .map(|k| k.label())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
        if self.warn_undeclared {
            for key in p.extensions.keys() {
                let declared = key == licence_key || self.fields.iter().any(|f| &f.name == key);
                if !declared {
                    push(
                        "undeclared_field",
                        Severity::Warning,
                        format!("extension field `{key}` is not declared in config/provenance.ron"),
                    );
                }
            }
        }
        out
    }
}

/// Lowercase label of a tool (`hand`, `agent`, `generator`, `import`, `unknown`).
#[must_use]
pub fn tool_label(tool: &ProvenanceTool) -> &'static str {
    match tool {
        ProvenanceTool::Hand => "hand",
        ProvenanceTool::Agent => "agent",
        ProvenanceTool::Generator => "generator",
        ProvenanceTool::Import => "import",
        ProvenanceTool::Unknown => "unknown",
    }
}

/// Whether the block carries `name` (an `Option` that is set and non-empty, a tool other than
/// `unknown`, a review state other than `pending`, or an extension key with a non-empty value).
#[must_use]
pub fn field_present(p: &Provenance, name: &str) -> bool {
    let set = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.trim().is_empty());
    match name {
        "origin" => set(&p.origin),
        "tool" => p.tool != ProvenanceTool::Unknown,
        "generator" => set(&p.generator),
        "seed" => p.seed.is_some(),
        "client" => set(&p.client),
        "session" => set(&p.session),
        "model" => set(&p.model),
        "created_utc" => set(&p.created_utc),
        "reviewer" => p.reviewer != ReviewerState::Pending,
        other => p.extensions.get(other).is_some_and(|v| !v.trim().is_empty()),
    }
}

/// One policy problem with one source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PolicyFinding {
    /// The source, project-relative (`assets/...`).
    pub source: String,
    /// Which rule: `missing_field`, `tool_not_allowed`, `origin_not_allowed`,
    /// `licence_not_allowed`, `value_not_allowed`, `unreviewed`, `undeclared_field` or
    /// `unreadable_meta`.
    pub rule: &'static str,
    /// `Error` refuses a final cook; `Warning` is reported only.
    pub severity: Severity,
    /// The human-readable text.
    pub text: String,
}

impl PolicyFinding {
    /// As a validator finding (code `provenance_policy`, the rule leading the text).
    #[must_use]
    pub fn to_finding(&self) -> Finding {
        Finding::new(
            PROVENANCE_POLICY_CODE,
            self.severity,
            format!("{}: {}", self.rule, self.text),
        )
        .with_hint("fix the provenance block in the asset's .meta, or the rule in config/provenance.ron")
    }
}

/// Load the project's policy; `Ok(None)` when it has no `config/provenance.ron`.
///
/// # Errors
///
/// The file exists but cannot be read or is invalid.
pub fn load_policy(project_root: &Path) -> Result<Option<ProvenancePolicy>, ProvenancePolicyError> {
    let path = project_root.join(PROVENANCE_POLICY_PATH);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ProvenancePolicyError::Io(e.to_string())),
    };
    ProvenancePolicy::from_ron(&text).map(Some)
}

/// Every `.meta` under `assets/` whose source exists: `(project-relative source, meta path)`,
/// sorted. Orphaned sidecars are left to [`crate::AssetDatabase::repair_orphaned_metas`].
#[must_use]
pub fn project_metas(project_root: &Path) -> Vec<(String, PathBuf)> {
    let assets = project_root.join("assets");
    let mut out = Vec::new();
    if !assets.is_dir() {
        return out;
    }
    for entry in walkdir::WalkDir::new(&assets)
        .into_iter()
        .filter_entry(|e| !e.file_name().to_string_lossy().starts_with('.') || e.depth() == 0)
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if !name.ends_with(".meta") || name.starts_with("._") {
            continue;
        }
        let source = path.with_extension("");
        if !source.is_file() {
            continue;
        }
        let rel = source
            .strip_prefix(project_root)
            .unwrap_or(&source)
            .to_string_lossy()
            .replace('\\', "/");
        out.push((rel, path.to_path_buf()));
    }
    out.sort();
    out
}

/// Check every source's `.meta` against `policy`. A `.meta` that does not parse is an
/// `unreadable_meta` error: an unreadable record is not a compliant one.
#[must_use]
pub fn check_project(project_root: &Path, policy: &ProvenancePolicy) -> Vec<PolicyFinding> {
    let _span = tracing::info_span!("provenance_policy_check").entered();
    let mut out = Vec::new();
    for (source, meta_path) in project_metas(project_root) {
        if policy.excludes(&source) {
            continue;
        }
        match std::fs::read(&meta_path).map(|b| MetaSidecar::from_ron(&b)) {
            Ok(Ok(meta)) => out.extend(policy.check_meta(&source, &meta)),
            Ok(Err(e)) => out.push(PolicyFinding {
                source,
                rule: "unreadable_meta",
                severity: Severity::Error,
                text: format!("the .meta does not parse: {e}"),
            }),
            Err(e) => out.push(PolicyFinding {
                source,
                rule: "unreadable_meta",
                severity: Severity::Error,
                text: format!("the .meta cannot be read: {e}"),
            }),
        }
    }
    out
}

/// The errors a final cook refuses on, or none when the project has no policy.
///
/// # Errors
///
/// The policy file exists but is invalid.
pub fn shipping_refusals(project_root: &Path) -> Result<Vec<PolicyFinding>, ProvenancePolicyError> {
    let Some(policy) = load_policy(project_root)? else {
        return Ok(Vec::new());
    };
    Ok(check_project(project_root, &policy)
        .into_iter()
        .filter(|f| f.severity == Severity::Error)
        .collect())
}

/// The `provenance_policy` validator: findings computed from the project's `.meta` files before
/// the run (validators never touch the disk), looked up by source path.
#[derive(Debug, Clone, Default)]
pub struct ProvenancePolicyValidator {
    findings: BTreeMap<String, Vec<PolicyFinding>>,
    checked: BTreeSet<String>,
}

impl ProvenancePolicyValidator {
    /// The validator for `project_root` under `policy`.
    #[must_use]
    pub fn for_project(project_root: &Path, policy: &ProvenancePolicy) -> Self {
        let mut v = Self::default();
        for (source, _) in project_metas(project_root) {
            v.checked.insert(source);
        }
        for f in check_project(project_root, policy) {
            v.findings.entry(f.source.clone()).or_default().push(f);
        }
        v
    }

    fn key<'a>(&self, path: &'a str) -> Option<&'a str> {
        if self.checked.contains(path) {
            return Some(path);
        }
        None
    }
}

impl Validator for ProvenancePolicyValidator {
    fn name(&self) -> &str {
        PROVENANCE_POLICY_CODE
    }

    fn description(&self) -> &str {
        "Source .meta provenance follows config/provenance.ron (required fields, tools, origins, licences, review)."
    }

    fn validate(&self, _ctx: &ValidationContext<'_>, asset: &AssetEntry) -> Verdict {
        if asset.path.contains('#') {
            return Verdict::not_validated("a sub-asset: its source's .meta carries the provenance");
        }
        let prefixed;
        let path = if asset.path.starts_with("assets/") {
            asset.path.as_str()
        } else {
            prefixed = format!("assets/{}", asset.path);
            prefixed.as_str()
        };
        let Some(key) = self.key(path) else {
            return Verdict::not_validated("no .meta next to the source");
        };
        let findings = self
            .findings
            .get(key)
            .map(|list| list.iter().map(PolicyFinding::to_finding).collect())
            .unwrap_or_default();
        Verdict::from_findings(findings)
    }
}

/// The versions a built-in migration step leaves (none: v1 is the first version).
#[must_use]
pub fn provenance_policy_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde field-list fingerprint of `config/provenance.ron` (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn provenance_policy_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<ProvenancePolicy>(),
        serde_field_names::<PolicyField>(),
        serde_field_names::<KindRule>(),
        serde_field_names::<ReviewRule>(),
        serde_variant_names::<ReviewKind>(),
        serde_variant_names::<ProvenanceTool>(),
    ])
}

/// The checked-in example, for tests and the fuzz fixture. The crate keeps its own copy (`cargo package`
/// copies only the crate folder, ADR-1485); a test holds it byte-identical to
/// `docs/formats/provenance.example.ron`.
pub const EXAMPLE_POLICY: &str = include_str!("../fixtures/provenance.example.ron");

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::AssetId;

    fn meta(importer: &str, provenance: Option<Provenance>) -> MetaSidecar {
        let mut m = MetaSidecar::new(importer, AssetId::new_random());
        m.provenance = provenance;
        m
    }

    fn block(tool: ProvenanceTool, reviewer: ReviewerState, ext: &[(&str, &str)]) -> Provenance {
        Provenance {
            tool,
            reviewer,
            origin: Some("dcc:blender".into()),
            extensions: ext.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect(),
            ..Provenance::default()
        }
    }

    fn rules(findings: &[PolicyFinding]) -> Vec<&'static str> {
        findings.iter().map(|f| f.rule).collect()
    }

    #[test]
    fn the_example_parses_and_round_trips() {
        let policy = ProvenancePolicy::from_ron(EXAMPLE_POLICY).expect("example");
        assert_eq!(policy.format_version, PROVENANCE_POLICY_FORMAT_VERSION);
        assert!(policy.fields.iter().any(|f| f.name == "foreign_game_file"));
        let again = ProvenancePolicy::from_ron(&policy.to_ron().expect("ron")).expect("re-read");
        assert_eq!(again, policy);
    }

    #[test]
    fn the_crate_example_matches_the_docs_example() {
        let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/formats/provenance.example.ron");
        let text = std::fs::read_to_string(&docs).expect("docs example");
        assert_eq!(text, EXAMPLE_POLICY, "keep both copies identical");
    }

    #[test]
    fn bad_policies_are_refused_with_invalid_config() {
        for (text, needle) in [
            ("(format_version: 2)", "newer"),
            ("(colour: 1)", "invalid"),
            ("(kinds: { \"png\": (required: [\"licence_typo\"]) })", "neither"),
            ("(fields: [(name: \"origin\")])", "built-in"),
            ("(fields: [(name: \"a\"), (name: \"a\")])", "twice"),
            ("(fields: [(name: \"Bad Name\")])", "lowercase"),
            ("(kinds: { \"PNG\": () })", "importer id"),
        ] {
            let err = ProvenancePolicy::from_ron(text).expect_err(text);
            assert_eq!(err.code(), "invalid_config");
            assert!(err.to_string().contains(needle), "{text}: {err}");
        }
    }

    #[test]
    fn required_fields_apply_per_kind_and_to_every_kind() {
        let policy = ProvenancePolicy::from_ron(
            "(fields: [(name: \"licence\")], kinds: { \"*\": (required: [\"origin\"]), \"png\": (required: [\"licence\", \"tool\"]) })",
        )
        .expect("policy");
        assert_eq!(policy.required_for("png"), vec!["origin", "licence", "tool"]);
        assert_eq!(policy.required_for("wav"), vec!["origin"]);
        let ok = meta(
            "png",
            Some(block(
                ProvenanceTool::Hand,
                ReviewerState::Pending,
                &[("licence", "CC0-1.0")],
            )),
        );
        assert!(policy.check_meta("assets/a.png", &ok).is_empty());
        let none = meta("png", None);
        let found = policy.check_meta("assets/b.png", &none);
        assert_eq!(rules(&found), vec!["missing_field"; 3]);
        assert!(found[0].text.contains("no provenance block"), "{}", found[0].text);
        // A wav needs only the origin.
        let wav = meta(
            "wav",
            Some(block(ProvenanceTool::Unknown, ReviewerState::Pending, &[])),
        );
        assert!(policy.check_meta("assets/c.wav", &wav).is_empty());
    }

    #[test]
    fn tools_origins_licences_values_and_review_are_enforced() {
        let policy = ProvenancePolicy::from_ron(
            r#"(
                fields: [(name: "foreign_game_file", values: ["false"])],
                warn_undeclared: true,
                tools: [hand, agent, import],
                origins: ["dcc:*", "https://polyhaven.com/**"],
                licences: ["CC0-1.0", "owner"],
                review: (required_for: [agent, generator]),
            )"#,
        )
        .expect("policy");
        let good = meta(
            "gltf",
            Some(block(
                ProvenanceTool::Agent,
                ReviewerState::Owner,
                &[("licence", "CC0-1.0"), ("foreign_game_file", "false")],
            )),
        );
        assert!(policy.check_meta("assets/a.glb", &good).is_empty());
        let mut bad = block(
            ProvenanceTool::Generator,
            ReviewerState::Pending,
            &[
                ("licence", "GPL-3.0"),
                ("foreign_game_file", "true"),
                ("note", "x"),
            ],
        );
        bad.origin = Some("ripped:othergame".into());
        let found = policy.check_meta("assets/b.glb", &meta("gltf", Some(bad)));
        assert_eq!(
            rules(&found),
            vec![
                "tool_not_allowed",
                "origin_not_allowed",
                "licence_not_allowed",
                "value_not_allowed",
                "unreviewed",
                "undeclared_field",
            ]
        );
        assert_eq!(found.last().map(|f| f.severity), Some(Severity::Warning));
        // A named reviewer counts by default; `accepted: [owner]` narrows it.
        let named = meta(
            "gltf",
            Some(block(
                ProvenanceTool::Agent,
                ReviewerState::Named("ana".into()),
                &[],
            )),
        );
        assert!(policy.check_meta("assets/c.glb", &named).is_empty());
        let strict = ProvenancePolicy::from_ron("(review: (required_for: [agent], accepted: [owner]))")
            .expect("strict");
        assert_eq!(
            rules(&strict.check_meta("assets/c.glb", &named)),
            vec!["unreviewed"]
        );
        // Excluded paths are never checked.
        let excluding =
            ProvenancePolicy::from_ron("(review: (required_for: [agent]), exclude: [\"assets/wip/**\"])")
                .expect("exclude");
        let pending = meta(
            "gltf",
            Some(block(ProvenanceTool::Agent, ReviewerState::Pending, &[])),
        );
        assert!(excluding.check_meta("assets/wip/x.glb", &pending).is_empty());
        assert_eq!(
            rules(&excluding.check_meta("assets/x.glb", &pending)),
            vec!["unreviewed"]
        );
    }

    #[test]
    fn a_project_without_a_policy_has_no_refusals() {
        let dir = regia_test_utils::unique_temp_dir("prov-policy-none");
        std::fs::create_dir_all(dir.join("assets")).expect("dir");
        let m = meta("png", None);
        std::fs::write(dir.join("assets/a.png"), b"x").expect("src");
        std::fs::write(dir.join("assets/a.png.meta"), m.to_ron_string().expect("ron")).expect("meta");
        assert!(load_policy(&dir).expect("no file").is_none());
        assert!(shipping_refusals(&dir).expect("no policy").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_check_reads_metas_and_flags_unreadable_ones() {
        let dir = regia_test_utils::unique_temp_dir("prov-policy-project");
        std::fs::create_dir_all(dir.join("assets/sub")).expect("dir");
        std::fs::create_dir_all(dir.join("config")).expect("dir");
        std::fs::write(
            dir.join(PROVENANCE_POLICY_PATH),
            "(kinds: { \"*\": (required: [\"origin\"]) })",
        )
        .expect("policy");
        let ok = meta(
            "png",
            Some(block(ProvenanceTool::Hand, ReviewerState::Pending, &[])),
        );
        let missing = meta("png", None);
        std::fs::write(dir.join("assets/ok.png"), b"x").expect("src");
        std::fs::write(dir.join("assets/ok.png.meta"), ok.to_ron_string().expect("ron")).expect("meta");
        std::fs::write(dir.join("assets/sub/no.png"), b"x").expect("src");
        std::fs::write(
            dir.join("assets/sub/no.png.meta"),
            missing.to_ron_string().expect("ron"),
        )
        .expect("meta");
        std::fs::write(dir.join("assets/bad.png"), b"x").expect("src");
        std::fs::write(dir.join("assets/bad.png.meta"), "not ron").expect("meta");
        // An orphaned sidecar (no source) is not checked here.
        std::fs::write(
            dir.join("assets/gone.png.meta"),
            missing.to_ron_string().expect("ron"),
        )
        .expect("meta");
        let refusals = shipping_refusals(&dir).expect("policy");
        let got: Vec<(&str, &str)> = refusals.iter().map(|f| (f.source.as_str(), f.rule)).collect();
        assert_eq!(
            got,
            vec![
                ("assets/bad.png", "unreadable_meta"),
                ("assets/sub/no.png", "missing_field"),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
