// SPDX-License-Identifier: MPL-2.0
//! `config/validation.ron` (v1) and the rule validators `path_style`, `size_budget` and
//! `dependency_rules` (spec 32 § 4.4, ADR-1668).
//!
//! The file carries the run options (`validate_on_save`, `cook`, `exclude`, `roots`), the existing
//! [`ValidationConfig`] limits and a list of [`Rule`]s. Loading is strict: an unknown field or variant is
//! a [`ValidationFileError`] (`invalid_config`) with the line and column in the message. The validators
//! read only the [`AssetSet`](crate::asset_graph::AssetSet) and its graph, never the disk.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::asset_graph::{AssetEntry, AssetKind};
use crate::lod_classes::glob_match;
use crate::validators::{
    Finding, LocalizedTwin, Severity, ValidationConfig, ValidationContext, Validator, ValidatorError,
    ValidatorRegistry, Verdict,
};

/// The `format_version` of `config/validation.ron`.
pub const VALIDATION_FILE_FORMAT_VERSION: u32 = 1;

/// What the cook does with validation findings (spec 32 § 4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CookGate {
    /// Do not validate.
    Off,
    /// Validate and report; the cook succeeds.
    #[default]
    Warn,
    /// Validate; a failing report fails the step.
    Fail,
}

impl CookGate {
    /// Whether validation runs at all.
    #[must_use]
    pub fn runs(self) -> bool {
        self != Self::Off
    }
}

/// Name styles for [`Rule::PathStyle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NameStyle {
    /// Lowercase ascii letters, digits and `_`.
    SnakeCase,
    /// Lowercase ascii letters, digits and `-`.
    KebabCase,
    /// Ascii only, no uppercase letters; digits, `_`, `-` and `.` allowed.
    LowerAscii,
}

impl NameStyle {
    fn accepts(self, c: char) -> bool {
        match self {
            Self::SnakeCase => c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_',
            Self::KebabCase => c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-',
            Self::LowerAscii => c.is_ascii() && !c.is_ascii_uppercase() && !c.is_ascii_control() && c != ' ',
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::SnakeCase => "snake_case",
            Self::KebabCase => "kebab-case",
            Self::LowerAscii => "lower ascii",
        }
    }
}

/// What part of the path a [`Rule::PathStyle`] judges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum NameTarget {
    /// The file stem (the last segment without its extension and `#fragment`).
    #[default]
    Stem,
    /// Every segment of the path (directories whole, the last segment as a stem).
    Path,
}

/// (De)serialize an `Option<T>` as a plain `T` when set, so rules read `style: SnakeCase` without `Some(..)`.
mod plain {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer, T: Serialize>(value: &Option<T>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => v.serialize(s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
        T::deserialize(d).map(Some)
    }
}

fn default_error() -> Severity {
    Severity::Error
}

/// One rule of `config/validation.ron`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Rule {
    /// Name style, affixes, characters and length of assets under a glob.
    PathStyle {
        /// Unique rule id.
        id: String,
        /// Assets the rule applies to.
        glob: String,
        /// Required style.
        #[serde(default, skip_serializing_if = "Option::is_none", with = "plain")]
        style: Option<NameStyle>,
        /// What the style judges.
        #[serde(default)]
        target: NameTarget,
        /// The stem must start with this.
        #[serde(default, skip_serializing_if = "Option::is_none", with = "plain")]
        prefix: Option<String>,
        /// The stem must end with this.
        #[serde(default, skip_serializing_if = "Option::is_none", with = "plain")]
        suffix: Option<String>,
        /// Only these characters are allowed in the stem.
        #[serde(default, skip_serializing_if = "Option::is_none", with = "plain")]
        allowed_chars: Option<String>,
        /// Longest stem in characters.
        #[serde(default, skip_serializing_if = "Option::is_none", with = "plain")]
        max_len: Option<usize>,
        /// Severity of the findings.
        #[serde(default = "default_error")]
        severity: Severity,
    },
    /// Cooked size and closure size limits.
    SizeBudget {
        /// Unique rule id.
        id: String,
        /// Assets the rule applies to (all when absent, together with `kind`).
        #[serde(default, skip_serializing_if = "Option::is_none", with = "plain")]
        glob: Option<String>,
        /// Asset kind the rule applies to.
        #[serde(default, skip_serializing_if = "Option::is_none", with = "plain")]
        kind: Option<AssetKind>,
        /// Most cooked bytes of the asset itself.
        #[serde(default, skip_serializing_if = "Option::is_none", with = "plain")]
        max_bytes: Option<u64>,
        /// Most cooked bytes of the asset plus everything it pulls in.
        #[serde(default, skip_serializing_if = "Option::is_none", with = "plain")]
        max_closure_bytes: Option<u64>,
        /// Most assets in the closure (the asset included).
        #[serde(default, skip_serializing_if = "Option::is_none", with = "plain")]
        max_closure_assets: Option<u64>,
        /// Severity of the findings.
        #[serde(default = "default_error")]
        severity: Severity,
    },
    /// `from` assets must not use `to` assets.
    Forbid {
        /// Unique rule id.
        id: String,
        /// Glob of the using assets.
        from: String,
        /// Glob of the forbidden targets.
        to: String,
        /// Severity of the findings.
        #[serde(default = "default_error")]
        severity: Severity,
    },
    /// Most direct dependencies.
    MaxDependencies {
        /// Unique rule id.
        id: String,
        /// Assets the rule applies to.
        glob: String,
        /// Most direct dependencies.
        max: usize,
        /// Severity of the findings.
        #[serde(default = "default_error")]
        severity: Severity,
    },
    /// No reference cycle through assets under the glob.
    NoCycles {
        /// Unique rule id.
        id: String,
        /// Assets the rule applies to.
        glob: String,
        /// Severity of the findings.
        #[serde(default = "default_error")]
        severity: Severity,
    },
}

impl Rule {
    /// The rule's id.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::PathStyle { id, .. }
            | Self::SizeBudget { id, .. }
            | Self::Forbid { id, .. }
            | Self::MaxDependencies { id, .. }
            | Self::NoCycles { id, .. } => id,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_version() -> u32 {
    VALIDATION_FILE_FORMAT_VERSION
}

/// Errors loading `config/validation.ron`; the stable code is always `invalid_config`.
#[derive(Debug, Error)]
pub enum ValidationFileError {
    /// The RON did not parse (the message has line and column) or has an unknown field or variant.
    #[error("config/validation.ron is invalid: {0}")]
    Parse(String),
    /// Written by a newer format.
    #[error("config/validation.ron format_version {found} is newer than the supported {supported}")]
    Version {
        /// Version in the file.
        found: u32,
        /// Newest version read.
        supported: u32,
    },
    /// Two rules share an id.
    #[error("config/validation.ron has two rules with the id `{0}`")]
    DuplicateRule(String),
    /// A rule is malformed (empty id, empty glob, zero limits).
    #[error("config/validation.ron rule `{id}`: {reason}")]
    BadRule {
        /// The rule id.
        id: String,
        /// What is wrong.
        reason: String,
    },
}

impl ValidationFileError {
    /// Stable lowercase code for the agent protocol and the CLI.
    #[must_use]
    pub fn code(&self) -> &'static str {
        "invalid_config"
    }
}

/// The serde shape of `config/validation.ron` for `cargo xtask check-formats`
/// (`docs/formats/validation-ron.md`, ADR-2316): the file, its `limits` table, the [`Rule`] variants
/// with their fields, and the spellings of every enum a rule reaches.
#[must_use]
pub fn validation_file_schema_fingerprint() -> String {
    use regia_core::schema::{
        join_field_lists, serde_field_names, serde_variant_names, serde_variant_shapes,
    };
    fn variants<T: Deserialize<'static>>() -> Vec<String> {
        let mut list = serde_variant_names::<T>();
        list.extend(serde_variant_shapes::<T>());
        list
    }
    join_field_lists(&[
        serde_field_names::<ValidationFile>(),
        variants::<CookGate>(),
        serde_field_names::<crate::validators::ValidationConfig>(),
        variants::<Rule>(),
        variants::<NameStyle>(),
        variants::<NameTarget>(),
        variants::<crate::validators::Severity>(),
    ])
}

/// `config/validation.ron` is at its first version: no migration steps.
#[must_use]
pub fn validation_file_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// `config/validation.ron` v1.
///
/// Source-lint thresholds are not stored here. They live in `config/source_lint.ron`
/// (`source_lint`, ADR-8248). This frozen file is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationFile {
    /// Format version ([`VALIDATION_FILE_FORMAT_VERSION`]).
    #[serde(default = "default_version")]
    pub format_version: u32,
    /// Queue a validation run after each level save. `false` queues nothing. A save that is not a
    /// level does not call `note_saved`. Default `true`.
    #[serde(default = "default_true")]
    pub validate_on_save: bool,
    /// What `regia cook` does with findings.
    #[serde(default)]
    pub cook: CookGate,
    /// Path globs left out of gathered scopes.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Reachability roots of the orphan check (default: scene assets).
    #[serde(default)]
    pub roots: Vec<String>,
    /// The limits of the built-in validators.
    #[serde(default)]
    pub limits: ValidationConfig,
    /// The rules.
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl Default for ValidationFile {
    fn default() -> Self {
        Self {
            format_version: VALIDATION_FILE_FORMAT_VERSION,
            validate_on_save: true,
            cook: CookGate::Warn,
            exclude: Vec::new(),
            roots: Vec::new(),
            limits: ValidationConfig::default(),
            rules: Vec::new(),
        }
    }
}

impl ValidationFile {
    /// Parse and check `config/validation.ron`.
    ///
    /// # Errors
    ///
    /// [`ValidationFileError`] for bad RON (unknown fields included), a newer version, a repeated rule
    /// id or a malformed rule.
    pub fn from_ron(text: &str) -> Result<Self, ValidationFileError> {
        let file: Self = ron::from_str(text).map_err(|e| ValidationFileError::Parse(e.to_string()))?;
        if file.format_version > VALIDATION_FILE_FORMAT_VERSION {
            return Err(ValidationFileError::Version {
                found: file.format_version,
                supported: VALIDATION_FILE_FORMAT_VERSION,
            });
        }
        let mut seen = BTreeSet::new();
        for rule in &file.rules {
            let id = rule.id();
            if id.is_empty() {
                return Err(ValidationFileError::BadRule {
                    id: String::new(),
                    reason: "the id is empty".into(),
                });
            }
            if !seen.insert(id) {
                return Err(ValidationFileError::DuplicateRule(id.to_owned()));
            }
            check_rule(rule)?;
        }
        Ok(file)
    }

    /// Serialize as pretty RON.
    ///
    /// # Errors
    ///
    /// [`ValidatorError::Write`] if serialization fails.
    pub fn to_ron(&self) -> Result<String, ValidatorError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| ValidatorError::Write(e.to_string()))
    }
}

fn check_rule(rule: &Rule) -> Result<(), ValidationFileError> {
    let bad = |reason: &str| {
        Err(ValidationFileError::BadRule {
            id: rule.id().to_owned(),
            reason: reason.to_owned(),
        })
    };
    match rule {
        Rule::PathStyle { glob, .. } | Rule::MaxDependencies { glob, .. } | Rule::NoCycles { glob, .. }
            if glob.is_empty() =>
        {
            bad("the glob is empty")
        }
        Rule::Forbid { from, to, .. } if from.is_empty() || to.is_empty() => {
            bad("`from` and `to` must not be empty")
        }
        Rule::SizeBudget {
            max_bytes: None,
            max_closure_bytes: None,
            max_closure_assets: None,
            ..
        } => bad("set at least one of max_bytes, max_closure_bytes, max_closure_assets"),
        _ => Ok(()),
    }
}

impl ValidatorRegistry {
    /// The six built-ins configured by `file.limits` plus `path_style`, `size_budget` and
    /// `dependency_rules` carrying `file.rules` (the rule validators report `not_validated` when no
    /// rule applies to an asset), `localized_twin` and `ui_image_missing`.
    #[must_use]
    pub fn with_rules(file: &ValidationFile) -> Self {
        let mut registry = Self::with_builtins(&file.limits);
        let extra: Vec<Box<dyn Validator>> = vec![
            Box::new(PathStyle::new(&file.rules)),
            Box::new(SizeBudget::new(&file.rules)),
            Box::new(DependencyRules::new(&file.rules)),
            Box::new(LocalizedTwin),
            // UI layouts' named pictures (ADR-3148).
            Box::new(crate::validators::UiImageMissing),
        ];
        for validator in extra {
            if let Err(e) = registry.register(validator) {
                tracing::error!(error = %e, "rule validator rejected");
            }
        }
        registry
    }
}

fn hint(id: &str) -> String {
    format!("rule `{id}` in config/validation.ron")
}

/// Judges names against the `PathStyle` rules.
#[derive(Debug, Clone)]
pub struct PathStyle {
    rules: Vec<Rule>,
}

impl PathStyle {
    /// Keep the `PathStyle` rules of `rules`.
    #[must_use]
    pub fn new(rules: &[Rule]) -> Self {
        Self {
            rules: rules
                .iter()
                .filter(|r| matches!(r, Rule::PathStyle { .. }))
                .cloned()
                .collect(),
        }
    }
}

/// The part of a last segment judged as the stem: no `#fragment`, no extension.
fn stem_of(segment: &str) -> &str {
    let no_fragment = segment.split('#').next().unwrap_or(segment);
    match no_fragment.rfind('.') {
        Some(i) if i > 0 => &no_fragment[..i],
        _ => no_fragment,
    }
}

impl Validator for PathStyle {
    fn name(&self) -> &str {
        "path_style"
    }

    fn description(&self) -> &str {
        "names follow the configured style, affixes, characters and length (config/validation.ron rules)"
    }

    fn validate(&self, _ctx: &ValidationContext<'_>, asset: &AssetEntry) -> Verdict {
        let mut applied = false;
        let mut findings = Vec::new();
        let segments: Vec<&str> = asset.path.split('/').collect();
        let last = segments.last().copied().unwrap_or("");
        let stem = stem_of(last);
        for rule in &self.rules {
            let Rule::PathStyle {
                id,
                glob,
                style,
                target,
                prefix,
                suffix,
                allowed_chars,
                max_len,
                severity,
            } = rule
            else {
                continue;
            };
            if !glob_match(glob, &asset.path) {
                continue;
            }
            applied = true;
            if let Some(style) = style {
                let judged: Vec<&str> = match target {
                    NameTarget::Stem => vec![stem],
                    NameTarget::Path => segments[..segments.len().saturating_sub(1)]
                        .iter()
                        .copied()
                        .chain(std::iter::once(stem))
                        .collect(),
                };
                for part in judged {
                    if let Some(bad) = part.chars().find(|c| !style.accepts(*c)) {
                        findings.push(
                            Finding::new(
                                "validate_name_style",
                                *severity,
                                format!(
                                    "`{part}` is not {}: `{}` is not allowed",
                                    style.label(),
                                    bad.escape_default()
                                ),
                            )
                            .with_hint(hint(id)),
                        );
                    }
                }
            }
            if let Some(chars) = allowed_chars
                && let Some(bad) = stem.chars().find(|c| !chars.contains(*c))
            {
                findings.push(
                    Finding::new(
                        "validate_name_style",
                        *severity,
                        format!(
                            "`{stem}` contains `{}`, which is not in the allowed characters",
                            bad.escape_default()
                        ),
                    )
                    .with_hint(hint(id)),
                );
            }
            if let Some(max) = max_len {
                let n = stem.chars().count();
                if n > *max {
                    findings.push(
                        Finding::new(
                            "validate_name_length",
                            *severity,
                            format!("`{stem}` is {n} characters long, over the limit of {max}"),
                        )
                        .with_hint(hint(id)),
                    );
                }
            }
            if let Some(p) = prefix
                && !stem.starts_with(p.as_str())
            {
                findings.push(
                    Finding::new(
                        "validate_name_prefix",
                        *severity,
                        format!("`{stem}` must start with `{p}`"),
                    )
                    .with_hint(hint(id)),
                );
            }
            if let Some(sfx) = suffix
                && !stem.ends_with(sfx.as_str())
            {
                findings.push(
                    Finding::new(
                        "validate_name_prefix",
                        *severity,
                        format!("`{stem}` must end with `{sfx}`"),
                    )
                    .with_hint(hint(id)),
                );
            }
        }
        if !applied {
            return Verdict::not_validated("no path_style rule matches this asset");
        }
        Verdict::from_findings(findings)
    }
}

/// Judges cooked sizes against the `SizeBudget` rules.
#[derive(Debug, Clone)]
pub struct SizeBudget {
    rules: Vec<Rule>,
}

impl SizeBudget {
    /// Keep the `SizeBudget` rules of `rules`.
    #[must_use]
    pub fn new(rules: &[Rule]) -> Self {
        Self {
            rules: rules
                .iter()
                .filter(|r| matches!(r, Rule::SizeBudget { .. }))
                .cloned()
                .collect(),
        }
    }
}

impl Validator for SizeBudget {
    fn name(&self) -> &str {
        "size_budget"
    }

    fn description(&self) -> &str {
        "cooked size and closure size stay within the configured budgets"
    }

    fn validate(&self, ctx: &ValidationContext<'_>, asset: &AssetEntry) -> Verdict {
        let mut applied = false;
        let mut findings = Vec::new();
        let mut closure: Option<(u64, u64)> = None;
        for rule in &self.rules {
            let Rule::SizeBudget {
                id,
                glob,
                kind,
                max_bytes,
                max_closure_bytes,
                max_closure_assets,
                severity,
            } = rule
            else {
                continue;
            };
            if glob.as_ref().is_some_and(|g| !glob_match(g, &asset.path))
                || kind.is_some_and(|k| k != asset.kind)
            {
                continue;
            }
            applied = true;
            if let Some(max) = max_bytes
                && asset.cooked_bytes > *max
            {
                findings.push(
                    Finding::new(
                        "validate_size_over_budget",
                        *severity,
                        format!(
                            "the asset is {} bytes, over the budget of {max}",
                            asset.cooked_bytes
                        ),
                    )
                    .with_hint(hint(id)),
                );
            }
            if max_closure_bytes.is_some() || max_closure_assets.is_some() {
                let (bytes, assets) = *closure.get_or_insert_with(|| {
                    let deps = ctx.graph.transitive_dependencies(&asset.path);
                    let bytes = deps
                        .iter()
                        .filter_map(|d| ctx.graph.own_bytes(d))
                        .fold(asset.cooked_bytes, u64::saturating_add);
                    (bytes, deps.len() as u64 + 1)
                });
                if let Some(max) = max_closure_bytes
                    && bytes > *max
                {
                    findings.push(
                        Finding::new(
                            "validate_closure_over_budget",
                            *severity,
                            format!(
                                "the asset and its dependencies are {bytes} bytes, over the budget of {max}"
                            ),
                        )
                        .with_hint(hint(id)),
                    );
                }
                if let Some(max) = max_closure_assets
                    && assets > *max
                {
                    findings.push(
                        Finding::new(
                            "validate_closure_over_budget",
                            *severity,
                            format!("the asset pulls in {assets} assets (itself included), over the budget of {max}"),
                        )
                        .with_hint(hint(id)),
                    );
                }
            }
        }
        if !applied {
            return Verdict::not_validated("no size_budget rule matches this asset");
        }
        Verdict::from_findings(findings)
    }
}

/// Judges references against the `Forbid`, `MaxDependencies` and `NoCycles` rules.
#[derive(Debug, Clone)]
pub struct DependencyRules {
    rules: Vec<Rule>,
}

impl DependencyRules {
    /// Keep the dependency rules of `rules`.
    #[must_use]
    pub fn new(rules: &[Rule]) -> Self {
        Self {
            rules: rules
                .iter()
                .filter(|r| {
                    matches!(
                        r,
                        Rule::Forbid { .. } | Rule::MaxDependencies { .. } | Rule::NoCycles { .. }
                    )
                })
                .cloned()
                .collect(),
        }
    }
}

impl Validator for DependencyRules {
    fn name(&self) -> &str {
        "dependency_rules"
    }

    fn description(&self) -> &str {
        "forbidden references, direct dependency counts and reference cycles (config/validation.ron rules)"
    }

    fn validate(&self, ctx: &ValidationContext<'_>, asset: &AssetEntry) -> Verdict {
        let mut applied = false;
        let mut findings = Vec::new();
        for rule in &self.rules {
            match rule {
                Rule::Forbid {
                    id,
                    from,
                    to,
                    severity,
                } if glob_match(from, &asset.path) => {
                    applied = true;
                    let mut targets: Vec<&str> = asset
                        .references
                        .iter()
                        .map(String::as_str)
                        .filter(|t| glob_match(to, t))
                        .collect();
                    targets.sort_unstable();
                    targets.dedup();
                    for t in targets {
                        findings.push(
                            Finding::new(
                                "validate_dependency_forbidden",
                                *severity,
                                format!("uses `{t}`, which `{from}` assets must not use"),
                            )
                            .with_hint(hint(id)),
                        );
                    }
                }
                Rule::MaxDependencies {
                    id,
                    glob,
                    max,
                    severity,
                } if glob_match(glob, &asset.path) => {
                    applied = true;
                    let n = ctx.graph.dependencies(&asset.path).len();
                    if n > *max {
                        findings.push(
                            Finding::new(
                                "validate_dependency_count",
                                *severity,
                                format!("the asset has {n} direct dependencies, over the limit of {max}"),
                            )
                            .with_hint(hint(id)),
                        );
                    }
                }
                Rule::NoCycles { id, glob, severity } if glob_match(glob, &asset.path) => {
                    applied = true;
                    if ctx.graph.in_cycle(&asset.path) {
                        findings.push(
                            Finding::new(
                                "validate_dependency_cycle",
                                *severity,
                                "the asset takes part in a reference cycle",
                            )
                            .with_hint(hint(id)),
                        );
                    }
                }
                _ => {}
            }
        }
        if !applied {
            return Verdict::not_validated("no dependency rule matches this asset");
        }
        Verdict::from_findings(findings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_graph::AssetSet;
    use crate::validators::{Outcome, RunSettings, ValidationReport};

    fn run(file: &ValidationFile, set: &AssetSet) -> ValidationReport {
        ValidatorRegistry::with_rules(file).run_with(
            set,
            &RunSettings {
                only: Some(vec![
                    "path_style".into(),
                    "size_budget".into(),
                    "dependency_rules".into(),
                ]),
                ..RunSettings::default()
            },
        )
    }

    fn file(text: &str) -> ValidationFile {
        ValidationFile::from_ron(text).expect("valid config")
    }

    fn codes(rep: &ValidationReport, asset: &str) -> Vec<String> {
        rep.results
            .iter()
            .filter(|r| r.asset == asset)
            .flat_map(|r| r.findings.iter().map(|f| f.code.clone()))
            .collect()
    }

    #[test]
    fn path_style_snake_kebab_and_non_ascii() {
        let f = file(r#"(rules: [PathStyle(id: "snake", glob: "assets/**", style: SnakeCase)])"#);
        let mut s = AssetSet::new();
        for p in [
            "assets/rock_a.png",
            "assets/Rock.png",
            "assets/rock-a.png",
            "assets/caf\u{e9}.png",
            "assets/deep/Dir/ok.png",
        ] {
            s.push(AssetEntry::new(p, AssetKind::Texture));
        }
        let rep = run(&f, &s);
        assert!(codes(&rep, "assets/rock_a.png").is_empty());
        assert_eq!(codes(&rep, "assets/Rock.png"), ["validate_name_style"]);
        assert_eq!(codes(&rep, "assets/rock-a.png"), ["validate_name_style"]);
        assert_eq!(codes(&rep, "assets/caf\u{e9}.png"), ["validate_name_style"]);
        // The default target is the stem, so a capitalized directory is fine.
        assert!(codes(&rep, "assets/deep/Dir/ok.png").is_empty());
        let kebab = file(r#"(rules: [PathStyle(id: "k", glob: "**", style: KebabCase)])"#);
        let rep = run(&kebab, &s);
        assert!(codes(&rep, "assets/rock-a.png").is_empty());
        let lower = file(r#"(rules: [PathStyle(id: "l", glob: "**", style: LowerAscii)])"#);
        let rep = run(&lower, &s);
        assert_eq!(codes(&rep, "assets/caf\u{e9}.png"), ["validate_name_style"]);
        assert!(codes(&rep, "assets/rock-a.png").is_empty());
    }

    /// ADR-7961: the built-in naming rule allows an uppercase stem. `PathStyle` `LowerAscii` (and
    /// `SnakeCase`) still report it as `validate_name_style`. Those codes are unchanged.
    ///
    /// Each set holds one path. Two paths that differ only by case are
    /// `validate_name_case_collision` on the built-in rule, which is a separate test.
    #[test]
    fn path_style_lower_ascii_still_flags_an_uppercase_stem() {
        let lower = file(
            r#"(rules: [PathStyle(id: "lowercase", glob: "assets/**", style: LowerAscii, severity: Error)])"#,
        );
        let mut upper = AssetSet::new();
        upper.push(AssetEntry::new("assets/SM_Rock.gltf", AssetKind::Mesh));
        let rep = ValidatorRegistry::with_rules(&lower).run(&upper);
        let naming = rep
            .results
            .iter()
            .find(|r| r.validator == "naming_rules" && r.asset == "assets/SM_Rock.gltf")
            .expect("naming row");
        assert_eq!(naming.outcome, Outcome::Pass, "{:?}", naming.messages);
        assert!(naming.findings.is_empty());
        assert_eq!(codes(&rep, "assets/SM_Rock.gltf"), ["validate_name_style"]);
        let mut lower_set = AssetSet::new();
        lower_set.push(AssetEntry::new("assets/sm_rock.gltf", AssetKind::Mesh));
        assert!(
            codes(
                &ValidatorRegistry::with_rules(&lower).run(&lower_set),
                "assets/sm_rock.gltf"
            )
            .is_empty()
        );
        let snake = file(
            r#"(rules: [PathStyle(id: "lowercase", glob: "assets/**", style: SnakeCase, severity: Error)])"#,
        );
        assert_eq!(
            codes(&run(&snake, &upper), "assets/SM_Rock.gltf"),
            ["validate_name_style"]
        );
    }

    #[test]
    fn path_style_path_target_judges_directories() {
        let f = file(r#"(rules: [PathStyle(id: "p", glob: "**", style: SnakeCase, target: Path)])"#);
        let mut s = AssetSet::new();
        s.push(AssetEntry::new("assets/Dir/ok.png", AssetKind::Texture));
        assert_eq!(codes(&run(&f, &s), "assets/Dir/ok.png"), ["validate_name_style"]);
    }

    #[test]
    fn path_style_length_prefix_and_suffix_boundaries() {
        let f = file(
            r#"(rules: [PathStyle(id: "n", glob: "assets/**", max_len: 6, prefix: "t_", suffix: "_d", severity: Warning)])"#,
        );
        let mut s = AssetSet::new();
        s.push(AssetEntry::new("assets/t_ab_d.png", AssetKind::Texture)); // 6 chars: at the limit, passes
        s.push(AssetEntry::new("assets/t_abc_d.png", AssetKind::Texture)); // 7 chars
        s.push(AssetEntry::new("assets/x_ab.png", AssetKind::Texture));
        let rep = run(&f, &s);
        assert!(codes(&rep, "assets/t_ab_d.png").is_empty());
        assert_eq!(codes(&rep, "assets/t_abc_d.png"), ["validate_name_length"]);
        assert_eq!(
            codes(&rep, "assets/x_ab.png"),
            ["validate_name_prefix", "validate_name_prefix"]
        );
        // Warnings pass the row but are counted.
        let row = rep
            .results
            .iter()
            .find(|r| r.asset == "assets/t_abc_d.png" && r.validator == "path_style");
        assert_eq!(row.map(|r| r.outcome), Some(Outcome::Pass));
        assert_eq!(rep.summary.warnings, 2);
    }

    #[test]
    fn path_style_allowed_chars() {
        let f = file(r#"(rules: [PathStyle(id: "c", glob: "**", allowed_chars: "abc")])"#);
        let mut s = AssetSet::new();
        s.push(AssetEntry::new("abc.png", AssetKind::Texture));
        s.push(AssetEntry::new("abd.png", AssetKind::Texture));
        let rep = run(&f, &s);
        assert!(codes(&rep, "abc.png").is_empty());
        assert_eq!(codes(&rep, "abd.png"), ["validate_name_style"]);
    }

    #[test]
    fn size_budget_boundary_and_kind() {
        let f = file(r#"(rules: [SizeBudget(id: "tex", kind: Texture, max_bytes: 100)])"#);
        let mut s = AssetSet::new();
        s.push(AssetEntry::new("a.png", AssetKind::Texture).with_cooked_bytes(100));
        s.push(AssetEntry::new("b.png", AssetKind::Texture).with_cooked_bytes(101));
        s.push(AssetEntry::new("c.mesh", AssetKind::Mesh).with_cooked_bytes(5000));
        let rep = run(&f, &s);
        assert!(codes(&rep, "a.png").is_empty());
        assert_eq!(codes(&rep, "b.png"), ["validate_size_over_budget"]);
        let mesh = rep
            .results
            .iter()
            .find(|r| r.asset == "c.mesh" && r.validator == "size_budget");
        assert_eq!(mesh.map(|r| r.outcome), Some(Outcome::NotValidated));
    }

    #[test]
    fn size_budget_closure_bytes_and_assets() {
        let f = file(
            r#"(rules: [SizeBudget(id: "lvl", glob: "levels/*", max_closure_bytes: 160, max_closure_assets: 2)])"#,
        );
        let mut s = AssetSet::new();
        s.push(
            AssetEntry::new("levels/a.scene", AssetKind::Scene)
                .with_cooked_bytes(10)
                .with_references(["m.mesh"]),
        );
        s.push(
            AssetEntry::new("m.mesh", AssetKind::Mesh)
                .with_cooked_bytes(100)
                .with_references(["t.png"]),
        );
        s.push(AssetEntry::new("t.png", AssetKind::Texture).with_cooked_bytes(51));
        let rep = run(&f, &s);
        // 161 bytes (over 160) and 3 assets (over 2): two findings.
        assert_eq!(
            codes(&rep, "levels/a.scene"),
            ["validate_closure_over_budget", "validate_closure_over_budget"]
        );
        let at_limit = file(
            r#"(rules: [SizeBudget(id: "lvl", glob: "levels/*", max_closure_bytes: 161, max_closure_assets: 3)])"#,
        );
        assert!(codes(&run(&at_limit, &s), "levels/a.scene").is_empty());
    }

    #[test]
    fn forbidden_edge_fan_out_and_cycle() {
        let f = file(
            r#"(rules: [
                Forbid(id: "ui", from: "ui/**", to: "dev/**"),
                MaxDependencies(id: "fan", glob: "prefabs/**", max: 2, severity: Warning),
                NoCycles(id: "cyc", glob: "cyc/**"),
            ])"#,
        );
        let mut s = AssetSet::new();
        s.push(AssetEntry::new("ui/a.ui", AssetKind::Other).with_references(["dev/x.png", "ui/b.ui"]));
        s.push(AssetEntry::new("ui/b.ui", AssetKind::Other));
        s.push(AssetEntry::new("dev/x.png", AssetKind::Texture));
        s.push(
            AssetEntry::new("prefabs/p.prefab", AssetKind::Other).with_references([
                "ui/b.ui",
                "ui/a.ui",
                "dev/x.png",
            ]),
        );
        s.push(AssetEntry::new("prefabs/q.prefab", AssetKind::Other).with_references(["ui/b.ui", "ui/a.ui"]));
        s.push(AssetEntry::new("cyc/a", AssetKind::Other).with_references(["cyc/b"]));
        s.push(AssetEntry::new("cyc/b", AssetKind::Other).with_references(["cyc/c"]));
        s.push(AssetEntry::new("cyc/c", AssetKind::Other).with_references(["cyc/a"]));
        s.push(AssetEntry::new("cyc/d", AssetKind::Other).with_references(["cyc/a"]));
        let rep = run(&f, &s);
        assert_eq!(codes(&rep, "ui/a.ui"), ["validate_dependency_forbidden"]);
        assert!(codes(&rep, "ui/b.ui").is_empty());
        assert_eq!(codes(&rep, "prefabs/p.prefab"), ["validate_dependency_count"]);
        assert!(codes(&rep, "prefabs/q.prefab").is_empty());
        for p in ["cyc/a", "cyc/b", "cyc/c"] {
            assert_eq!(codes(&rep, p), ["validate_dependency_cycle"], "{p}");
        }
        assert!(codes(&rep, "cyc/d").is_empty());
    }

    #[test]
    fn config_unknown_field_has_line_and_duplicate_ids_are_refused() {
        let err =
            ValidationFile::from_ron("(\n  format_version: 1,\n  bogus: 3,\n)").expect_err("unknown field");
        assert_eq!(err.code(), "invalid_config");
        assert!(err.to_string().contains("3:"), "{err}");
        let err = ValidationFile::from_ron(
            r#"(rules: [PathStyle(id: "a", glob: "x"), NoCycles(id: "a", glob: "y")])"#,
        )
        .expect_err("duplicate");
        assert!(matches!(err, ValidationFileError::DuplicateRule(ref id) if id == "a"));
        let err = ValidationFile::from_ron(r#"(rules: [Nope(id: "a")])"#).expect_err("unknown variant");
        assert_eq!(err.code(), "invalid_config");
        let err = ValidationFile::from_ron("(format_version: 9)").expect_err("newer");
        assert!(matches!(err, ValidationFileError::Version { found: 9, .. }));
        let err = ValidationFile::from_ron(r#"(rules: [SizeBudget(id: "s")])"#).expect_err("empty budget");
        assert!(matches!(err, ValidationFileError::BadRule { .. }));
    }

    #[test]
    fn config_defaults_and_spec_example_round_trip() {
        let d = file("()");
        assert_eq!(d, ValidationFile::default());
        assert!(d.validate_on_save);
        assert_eq!(d.cook, CookGate::Warn);
        assert_eq!(d.limits, ValidationConfig::default());
        let example = file(
            r#"(
            format_version: 1,
            validate_on_save: true,
            cook: Warn,
            exclude: ["assets/dev/**"],
            roots: ["assets/levels/**"],
            limits: (max_texture_dimension: 4096, texture_power_of_two: false, mesh_triangle_budget: 200000, max_path_len: 200),
            rules: [
                PathStyle(id: "lowercase", glob: "assets/**", style: SnakeCase, severity: Error),
                SizeBudget(id: "texture-cap", kind: Texture, max_bytes: 16777216, severity: Warning),
                Forbid(id: "ui-not-dev", from: "assets/ui/**", to: "assets/dev/**", severity: Error),
            ],
        )"#,
        );
        let text = example.to_ron().expect("writes");
        assert_eq!(ValidationFile::from_ron(&text).expect("reads"), example);
    }

    #[test]
    fn rule_validators_are_listed_either_way() {
        let names = ValidatorRegistry::with_rules(&ValidationFile::default()).names();
        for n in ["path_style", "size_budget", "dependency_rules", "localized_twin"] {
            assert!(names.iter().any(|x| x == n), "{n}");
        }
        assert_eq!(
            ValidatorRegistry::with_builtins(&ValidationConfig::default())
                .names()
                .len(),
            6
        );
    }
}
