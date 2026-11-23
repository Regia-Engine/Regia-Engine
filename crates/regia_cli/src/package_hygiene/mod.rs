// SPDX-License-Identifier: MPL-2.0
//! Configurable shipping-package hygiene (ADR-8062).
//!
//! Original Rust from the C-09 requirements review of the three approved DO27 packaging
//! tools at d6ed37677 (regia-absorb-1); no upstream implementation or game terms copied.
//! Files are scanned as bytes before packaging, with decoded `.fpak` entries also checked.
//! Findings never contain matched text or the configured patterns. Run against an immutable
//! staging tree: this is a packaging gate, not a sandbox for concurrently hostile writers.
//!
//! After packaging, [`check_artifact`] inspects the final generated archive in place (ZIP, APK,
//! AAB, tar, tar.gz, nested `.fpak`) and [`write_receipt`] records a versioned [`Receipt`] bound
//! to the artifact hash, policy digest and tool version (ADR-8348).

mod host_strings;
mod policy;
mod receipt;
mod scan;
mod stage;

pub use host_strings::check_executable_strings;

pub use policy::{
    Allowance, EffectiveAllowance, EffectivePolicy, EffectiveRule, OBSOLETE_ALLOWANCE, PathScrub, Policy,
    Rule, StageRules, definition_digest,
};
pub use receipt::{
    ArtifactOptions, ContainerCoverage, Coverage, RECEIPT_SCHEMA, RECEIPT_SUFFIX, RECEIPT_VERSION, Receipt,
    SUPPORTED_CONTAINERS, UninspectedEntry, audit_artifact, check_artifact, receipt_path, remove_receipt,
    tool_version, verify_receipt, write_receipt,
};

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// One rejected rule at its first location in a file, name, or decoded pak entry.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Finding {
    /// Privacy-scrubbed display path, never an allowance or filesystem lookup path.
    /// Pak entries use `<scrubbed-pak>::<asset-id>`.
    pub path: String,
    /// Opaque scan-local identity, independent of filename bytes. Not stable across tree edits.
    /// A policy finding uses `policy-<index>` and is not a filesystem location.
    pub location_id: String,
    /// Stable rule ID. List policies still number configurable rules from one.
    /// Explicit rules use their `id`. Built-in secret IDs do not change.
    /// [`OBSOLETE_ALLOWANCE`] marks an allowance whose definition no longer matches.
    pub rule: String,
    /// `content`, `name`, or `policy` (an obsolete allowance, not a staged byte).
    pub source: String,
    /// Zero-based byte offset within the content or original path, not the scrubbed display.
    pub offset: usize,
}

/// Byte and work ceilings in force for one audit. Copied onto every report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct AuditLimits {
    /// Peak bytes charged for scan buffers and decoded entries.
    pub max_memory_bytes: u64,
    /// Maximum bytes of one file or decompressed pak entry.
    pub max_file_bytes: u64,
    /// Maximum total scanned file and decompressed-entry bytes.
    pub max_total_bytes: u64,
    /// Bytes the audit may present to matchers. Overlap is not charged twice.
    pub max_work: u64,
    /// Window used to stream literals and bounded patterns.
    pub scan_window_bytes: u64,
    /// Sum of per-rule compiled-matcher reservations.
    pub max_matcher_bytes: u64,
}

/// Deterministic audit summary. An empty finding list on a complete scan means the package passed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    /// Regular files scanned (pak containers included).
    pub files_scanned: u64,
    /// Decoded pak entries scanned in addition to the files.
    pub pak_entries_scanned: u64,
    /// Bytes scanned, including decoded pak entries.
    pub bytes_scanned: u64,
    /// Hits covered by explicit reviewed exceptions.
    pub allowed_findings: u64,
    /// Completed rewrites and drops. These do not fail the package.
    pub actions: Vec<Finding>,
    /// Rejected findings, sorted by display path, location ID, rule, source and offset.
    pub findings: Vec<Finding>,
    /// BLAKE3 of the effective policy (`b3:` plus hex). It contains no rule text.
    pub policy_digest: String,
    /// False when the audit stopped before every selected byte was checked.
    pub complete: bool,
    /// Why `complete` is false. Empty when the audit finished.
    ///
    /// `memory_budget`, `file_bytes`, `total_bytes`, `work_budget`, `entry_count`,
    /// `finding_cap`, `depth`, `unbounded_regex`, `decompression_bomb`, `pak_resident`,
    /// `nested_limit`, `pattern_wider_than_memory`, `changed_file`, `read_error`, `path`, `pak`;
    /// final-artifact audits add `archive`, `duplicate_name` and `traversal_name`.
    pub incomplete_reason: String,
    /// Limits copied from the policy, including defaults.
    pub limits: AuditLimits,
    /// Peak logical bytes charged for retained scan buffers and decoded entries.
    pub peak_memory_bytes: u64,
    /// Bytes presented to matchers. A window's overlap is not charged again.
    pub work: u64,
    /// Bytes reserved for compiled matchers.
    pub matcher_bytes: u64,
    /// Matchers compiled for this policy.
    pub matchers_compiled: u32,
}

/// An invalid policy, incomplete scan, or rejected package. No matched content is retained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditError {
    /// `package_hygiene_config`, `package_hygiene_io`, `package_hygiene_path`,
    /// `package_hygiene_limit`, `package_hygiene_pak`, or `package_hygiene_failed`. Final-artifact
    /// audits add `package_hygiene_archive`, `package_hygiene_uninspected` and
    /// `package_hygiene_receipt`.
    pub code: &'static str,
    /// Privacy-scrubbed display path or a fixed policy field identifying the failure.
    pub location: String,
    /// Opaque scan-local identity for a filesystem or pak location. Absent before traversal
    /// (policy/manifest errors) and for the aggregate rejection of a completed scan.
    pub location_id: Option<String>,
    /// Completed report on a policy rejection. Partial report when a scan stops after it has
    /// started. Absent when the audit did not start, and absent for a file-size admission
    /// (`file_bytes`) that fires before a scan buffer exists. Boxed so the error stays small.
    pub report: Option<Box<Report>>,
    /// Bytes reserved for compiled matchers when this error is a matcher budget.
    pub matcher_bytes: u64,
    /// Matchers compiled before this error.
    pub matchers_compiled: u32,
}

impl fmt::Display for AuditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.location)?;
        if let Some(id) = &self.location_id {
            write!(f, " [{id}]")?;
        }
        if let Some(report) = &self.report {
            for hit in &report.findings {
                write!(
                    f,
                    "\n  {} [{}]: {} at {} byte {}",
                    hit.path, hit.location_id, hit.rule, hit.source, hit.offset
                )?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for AuditError {}

impl AuditError {
    fn new(code: &'static str, location: impl Into<String>) -> Self {
        Self {
            code,
            location: location.into(),
            location_id: None,
            report: None,
            matcher_bytes: 0,
            matchers_compiled: 0,
        }
    }

    fn config(location: impl Into<String>) -> Self {
        Self::new("package_hygiene_config", location)
    }
}

impl Policy {
    /// Read and validate a project's policy using the existing project-manifest name resolver.
    /// Reads at most 1 MiB plus one byte and never exposes parser diagnostics or policy values.
    /// Package builders should call this before general manifest decoding, for every profile.
    ///
    /// # Errors
    /// Missing/unreadable manifest, invalid encoding, excessive size, or invalid policy.
    pub fn from_project(project: &Path) -> Result<Self, AuditError> {
        Self::from_manifest(&policy::read_manifest(project)?)
    }

    /// Parse the optional `[package_hygiene]` table of a project manifest and validate it.
    /// No other manifest fields are inspected. Missing table uses the default policy.
    ///
    /// # Errors
    /// Invalid TOML, unknown policy fields, invalid rules, exceptions, or limits. Parse errors
    /// are redacted because a forbidden pattern may itself contain sensitive material.
    pub fn from_manifest(text: &str) -> Result<Self, AuditError> {
        if text.len() > 1 << 20 {
            return Err(AuditError::config("manifest_size"));
        }
        #[derive(Deserialize)]
        struct Manifest {
            #[serde(default)]
            package_hygiene: Policy,
        }
        let mut policy = toml::from_str::<Manifest>(text)
            .map_err(|_| AuditError::config("Regia.toml [package_hygiene]"))?
            .package_hygiene;
        policy.bind_loaded()?;
        policy.validate()?;
        Ok(policy)
    }

    /// Validate limits, expressions and exception references without reading staged files.
    ///
    /// # Errors
    /// An invalid or excessive rule, limit, or exception.
    pub fn validate(&self) -> Result<(), AuditError> {
        policy::Compiled::new(self).map(drop)
    }
}

/// Audit every entry in a staged directory, including decoded `.fpak` payloads.
///
/// No ignore files or repository exclusions apply. Symlinks, special files, non-UTF-8 names,
/// unreadable files, excessive inputs and malformed or undecodable paks are errors, never skipped successes.
/// Reports include at most one finding per rule/source/entry; they contain no content excerpts.
///
/// # Errors
/// Policy invalid, root unavailable, unsafe entry, read/decode failure or resource limit.
pub fn audit_tree(root: &Path, policy: &Policy) -> Result<Report, AuditError> {
    scan::tree(root, policy, &mut |_, _| {})
}

/// Audit with an explicitly authorized local review callback receiving `(location_id, path)`.
///
/// The callback sees exact normalized relative paths, including private filenames. Keep it
/// local: never forward these values to logs, telemetry, JSON reports or agent responses.
/// Normal CLI and agent audits do not use this callback. Pak locations contain asset IDs,
/// never manifest names. Unsupported names are refused without revealing them.
///
/// IDs use sorted traversal ordinals. Rerun against the same immutable staging tree and
/// policy to resolve an ID; they are not persistent identities across edits. The callback
/// may run before a later incomplete-scan error and must not treat visitation as acceptance.
///
/// # Errors
/// The same invalid policy, path, read, decode and resource errors as [`audit_tree`].
pub fn audit_tree_with_locations(
    root: &Path,
    policy: &Policy,
    local_review: &mut dyn FnMut(&str, &str),
) -> Result<Report, AuditError> {
    scan::tree(root, policy, local_review)
}

/// Validate a staged package before it is archived.
///
/// # Errors
/// Any incomplete audit or rejected finding. On rejection the error carries the full report.
pub fn check_tree(root: &Path, policy: &Policy) -> Result<Report, AuditError> {
    let report = audit_tree(root, policy)?;
    if report.findings.is_empty() {
        Ok(report)
    } else {
        Err(AuditError {
            code: "package_hygiene_failed",
            location: "staged package rejected".into(),
            location_id: None,
            matcher_bytes: report.matcher_bytes,
            matchers_compiled: report.matchers_compiled,
            report: Some(Box::new(report)),
        })
    }
}

/// Load a project's policy and stage rules, then check its staging tree.
///
/// The stage-rule pass reports build paths, allowlist misses, leftover internal notes and
/// dev-feature markers. It does not rewrite, delete or execute files. Shipping preparation
/// is [`prepare_project_tree`].
///
/// # Errors
/// Cannot read the manifest, invalid policy or stage rules, incomplete scan, or a rejected finding.
pub fn check_project_tree(project: &Path, staging: &Path) -> Result<Report, AuditError> {
    let policy = Policy::from_project(project)?;
    let stage = StageRules::from_project(project)?;
    let allow_file = host_strings::project_allow_file(project)?;
    let hosts = host_strings::Scanner::new(allow_file.as_deref())?;
    let mut report = audit_tree(staging, &policy)?;
    stage::review(staging, &policy, &stage, &mut report)?;
    stage::review_hosts(staging, &policy, &hosts, &mut report)?;
    if report.findings.is_empty() {
        Ok(report)
    } else {
        Err(AuditError {
            code: "package_hygiene_failed",
            location: "staged package rejected".into(),
            location_id: None,
            report: Some(Box::new(report)),
            matcher_bytes: 0,
            matchers_compiled: 0,
        })
    }
}

/// Load the project policy and stage rules, then prepare `staging` for a shipping archive.
///
/// Drops internal notes, rewrites or rejects build paths, enforces the allowlist and probes
/// dev-feature markers. Does not run the content-string audit; call [`check_project_tree`]
/// after this returns.
///
/// # Errors
/// Invalid policy or stage rules, an unreadable tree, or a rejected stage rule.
pub fn prepare_project_tree(project: &Path, staging: &Path) -> Result<Report, AuditError> {
    let policy = Policy::from_project(project)?;
    let stage = StageRules::from_project(project)?;
    prepare_stage(staging, &policy, &stage)
}

/// Drop internal notes, scrub build paths, enforce the stage allowlist and probe dev markers.
///
/// Shipping package preparation calls this. It rewrites and deletes files.
/// [`check_project_tree`] reports the same rules without writing, deleting or executing.
///
/// # Errors
/// Policy invalid, root unavailable, or a rejected rule. On rejection the error carries the report.
pub fn prepare_stage(root: &Path, policy: &Policy, stage: &StageRules) -> Result<Report, AuditError> {
    stage::prepare(root, policy, stage)
}

/// Installs a thread-local hook called after a file's size and modification time are recorded
/// and before its bytes are read. The audit does not clear the hook. Production callers leave
/// it unset; tests use it to change a file without sleeping.
pub fn set_mutation_probe(probe: Option<fn(&Path)>) {
    scan::set_mutation_probe(probe);
}

/// When `owned` is true, a pak is read into a charged buffer instead of a file mapping.
/// The audit does not clear the flag. Production callers leave it false.
pub fn set_pak_owned_reads(owned: bool) {
    scan::set_pak_owned_reads(owned);
}

#[cfg(test)]
mod tests;
