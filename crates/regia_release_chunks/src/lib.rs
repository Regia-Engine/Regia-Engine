// SPDX-License-Identifier: MPL-2.0
//! Release chunks under a byte cap (`release-chunks` format version 1, ADR-8404).
//!
//! Rewritten in Rust from the behavior of the release packaging tools at pin `d6ed37677`
//! (`regia-absorb-1`). Group names, size caps and directory prefixes are settings. Identifiers
//! from that tree are not copied.
//!
//! Public algorithms: POSIX ustar (IEEE Std 1003.1 `pax`), SHA-256 and SHA-1 (FIPS 180-4),
//! FNV-1a 64, and zstd frames (RFC 8878) via the `zstd` program. Release signatures use the
//! workspace crates `ring` (Ed25519) and `blake3` (key id). SHA-256 and SHA-1 stay in this crate.
//!
//! `verify_release`, `diff_release` and `upload_plan` follow the release verify, diff and
//! upload-plan requirements at pin `c9513623f` (`regia-absorb-4`, ADR-8557). Python was not copied.
//! `sign_release` and `upload_with_backend` follow the release-sign requirements at the same pin
//! (ADR-8687). The shell script was not copied. `CliBackend` uploads through a hosting CLI run
//! as a child process with its arguments from an `upload_cli = 1` config (ADR-8846).
//!
//! Every written chunk is at most [`ChunkSettings::max_chunk_bytes`], and that cap is always
//! below [`TWO_GIB`]. A group that does not fit is split until each piece fits, or refused
//! with `chunk_over_cap` when one file cannot be split. Nothing over the cap is left behind.

mod codec;
mod cut;
mod diff;
mod hash;
mod manifest;
mod scan;
mod sign;
mod tar;
mod upload;
mod upload_cli;
mod upload_plan;
mod verify;

pub use codec::{ChunkCodec, Identity, ZstdCli};
pub use cut::{
    CutRequest, PackRequest, PlannedGroup, ReleasePlan, VerifyReport, audit_tree, cut_tree, pack_release,
    plan_tree, plan_tree_compressed, verify_tree, write_dir_prints,
};
pub use diff::{
    ChangedChunk, ChunkDiff, ChunkView, DiffOptions, DiffReport, FileDiff, FileEntry, GroupDiff, MovedFile,
    PairDiff, diff_release, directory_group, format_markdown, format_size, format_size_detailed, format_text,
};
pub use hash::{bucket_index, content_hash, fnv1a64, mesh_id, sha1, sha256, sha256_hex};
pub use manifest::{
    AuditSettings, ChunkLink, ChunkManifest, ChunkRecord, DirPrint, FORMAT_NAME, canonical_example,
    migrate_json, peek_version, read_manifest, schema_fingerprint,
};
pub use scan::{ChunkSettings, GroupRule, GroupRules, load_rules_str, parse_byte_size};
pub use sign::{
    ManifestSignStatus, RELEASE_SIGNATURE_FORMAT, RELEASE_SIGNATURE_FORMAT_NAME, RELEASE_SIGNATURE_SUFFIX,
    SignReport, SignStatusReport, canonical_signature_json, generate_throwaway_signing_key,
    peek_signature_version, sign_release, sign_status, signature_schema_fingerprint,
};
pub use upload::{
    BackendSpec, FakeBackend, FsBackend, RemoteFile, UploadBackend, UploadReceipt, parse_backend,
    parse_fs_backend, upload_with_backend,
};
pub use upload_cli::{
    CliBackend, CliCall, CliDryRun, MIN_SECRET_LEN, UPLOAD_CLI_FORMAT, UploadCliConfig, cli_dry_run,
    parse_listing, redact, resolve_secrets,
};
pub use upload_plan::{
    AttemptCount, GateGroup, GateOutcome, LoadedPlan, PlanGroup, UPLOAD_PLAN_SCHEMA, UploadGroup, UploadPlan,
    UploadPlanOptions, attempts_until_success, expand_patterns, gated_uploads, is_manifest_name,
    load_manifest_assets, load_plan_str, publish_gate_ok, repo_ok, size_mismatch_message, skip_same_size,
    stable_size, tag_ok, upload_plan, upload_plan_with,
};
pub use verify::{CheckedManifest, ReleaseVerifyReport, VerifyOptions, VerifyProblem, verify_release};

/// Format version of `release-chunks` JSON. The first version; a newer file is refused.
pub const FORMAT_VERSION: u32 = 1;

/// Absolute ceiling. A configured cap at or above this is `chunk_cap_invalid`.
pub const TWO_GIB: u64 = 1024u64 * 1024 * 1024 * 2;

/// Default compressed-chunk cap: 1.9 GiB, under [`TWO_GIB`].
pub const DEFAULT_MAX_CHUNK_BYTES: u64 = 1024u64 * 1024 * 1024 * 19 / 10;

/// Default raw-byte target for one release bucket: 1.6 GiB.
pub const DEFAULT_BUCKET_RAW_BYTES: u64 = 1024u64 * 1024 * 1024 * 8 / 5;

/// A failure of planning, packing or verification.
///
/// `code` is lowercase ASCII, digits and `_`, and is part of the agent surface
/// (`book/agents/release-chunks.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkError {
    /// Stable error code.
    pub code: &'static str,
    /// What failed, and the path or size when that helps.
    pub message: String,
}

impl ChunkError {
    /// An error with `code`.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// An IO failure on `path` (`io_error`).
    #[must_use]
    pub fn io(path: &std::path::Path, err: &std::io::Error) -> Self {
        Self::new("io_error", format!("{}: {err}", path.display()))
    }
}

impl std::fmt::Display for ChunkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for ChunkError {}

#[cfg(test)]
mod release_ops_tests;
#[cfg(test)]
// A `tests.rs` file, so `cargo xtask check-packaging` treats its repo-level fixture reads as test code
// (ADR-1485).
#[path = "sign_tests/tests.rs"]
mod sign_tests;
#[cfg(test)]
mod tests;
