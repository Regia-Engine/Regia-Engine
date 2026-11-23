// SPDX-License-Identifier: MPL-2.0
//! Content chunks, DLC paks and chunk delivery (`docs/specs/40-content-chunks-dlc-asset-packs.md`).
//!
//! The cook (`regia_asset_pipeline::chunks`) assigns assets to named chunks, writes one pak per
//! chunk plus a [`ContentManifest`] and a [`ChunkMap`]. At run time [`ContentManager`] knows every
//! chunk's [`ChunkState`], fetches missing chunks through a [`ChunkSource`], verifies them by
//! BLAKE3, and mounts them into the [`crate::AssetServer`] in manifest mount order. An asset whose
//! chunk is not mounted loads as `LoadState::Failed("unavailable:<chunk>")` and
//! [`crate::AssetServer::unavailable_chunk`] names the chunk.
//!
//! A project with no manifest behaves exactly as before: [`ContentPlugin`] is a no-op without
//! `content-manifest.ron`.
//!
//! Transfers are stepped by byte budgets ([`ContentManager::advance`]), so a test drives every
//! state transition in a fixed order without sleeping (spec § 7).

mod host;
mod http;
mod manager;
mod manifest;
mod source;
mod store;
mod trust;
mod why;

pub use host::{
    CONTENT_BASE_URL_ENV, CONTENT_HOST_FILE, CONTENT_HOST_FORMAT_VERSION, ContentHost,
    content_host_schema_fingerprint, pick_base_url, resolve_base_url,
};
pub use http::{
    CONTENT_ALLOW_HTTP_ENV, FetchedManifest, HttpClient, HttpSource, HttpUrl, ServeStats, fetch_manifest,
    fetch_manifest_trusted, fetch_manifest_with, plain_http_allowed_by_env, serve_connection, serve_dir,
    serve_dir_loopback,
};
pub use manager::{
    ChunkStateChanged, ContentFault, ContentManager, ContentPlugin, ContentScanCounts, ContentSettings,
    METERED_POLL_TICKS, ManifestUpdate, MeteredProbe, NetworkCost, RequestId, RequestOptions,
    RequestProgress, SpaceProbe, Tier, VerifyRunner, content_queue_walks, content_state_key_allocs,
    content_tick, io_pool_verify_runner, platform_metered_probe, reset_content_queue_walks,
    reset_content_state_key_allocs,
};
pub use manifest::{
    BASE_CHUNK, CHUNK_ENTRY_SUFFIX, CHUNK_FILE_EXTENSION, CHUNK_MAP_FILE, CHUNK_MAP_FORMAT_VERSION,
    CHUNK_MAP_MAGIC, CONTENT_MANIFEST_FILE, CONTENT_MANIFEST_FORMAT_VERSION, CONTENT_STATE_FILE,
    CONTENT_STATE_FORMAT_VERSION, ChunkEntry, ChunkMap, ContentManifest, ContentState, DLC_CHUNK_MAP_SUFFIX,
    Delivery, InstalledChunk, MAX_BUILD_ID_LEN, MAX_CHUNK_NAME_LEN, chunk_entry_clones,
    chunk_map_schema_fingerprint, content_manifest_migration_sources, content_manifest_schema_fingerprint,
    content_state_migration_sources, content_state_schema_fingerprint, hash_file, mount_order, pack_name,
    reset_chunk_entry_clones, validate_build_id, validate_chunk_name,
};
pub use source::{
    ChunkSource, DirSource, LocalSource, RESUME_BLOCK, SourceStatus, Transfer, TransferStep, block_hashes,
};
pub use store::{PackStore, PlatformPackStore, STORE_UNKNOWN_STATUS, StorePackSource, chunk_state_for};
#[cfg(feature = "content-signing")]
pub use trust::SigningKey;
pub use trust::{
    CONTENT_SIGNATURE_FILE, CONTENT_SIGNATURE_FORMAT_VERSION, CONTENT_TRUST_FILE,
    CONTENT_TRUST_FORMAT_VERSION, ContentSignature, ContentTrust, MAX_SIGNATURE_BYTES, MAX_TRUSTED_KEYS,
    PUBLIC_KEY_PREFIX, SIGNATURE_ALGORITHM, SIGNATURE_DOMAIN, SIGNING_KEY_HEADER, SignatureCheck,
    SignaturePolicy, check_manifest_signature, content_signature_schema_fingerprint,
    content_trust_schema_fingerprint, format_public_key, key_id, parse_public_key, signed_message,
};
pub use why::{
    CHUNK_WHY_FILE, CHUNK_WHY_FORMAT_VERSION, CHUNK_WHY_MAGIC, ChunkWhy, WhyAnswer, WhyReason, WhyRecord,
    chunk_why_schema_fingerprint,
};

/// Why a chunk failed (`ChunkState::Failed`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailReason {
    /// The file's size or BLAKE3 did not match the manifest. Failed downloads are deleted;
    /// a rejected installed file is kept for diagnosis or replacement.
    HashMismatch,
    /// Not enough free space for the missing bytes plus 5 percent; nothing was written.
    NoSpace,
    /// The source could not deliver the chunk.
    SourceUnreachable(String),
    /// An Android store status this build does not map.
    StoreUnknownStatus,
    /// A fault armed with `content.inject`.
    Injected,
    /// A file system error.
    Io(String),
    /// The file's length is not the manifest's `size` (a torn or half-written copy): it is not
    /// mounted (ADR-8810).
    SizeMismatch,
}

impl FailReason {
    /// The agent `data.reason` code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::HashMismatch => "hash_mismatch",
            Self::NoSpace => "no_space",
            Self::SourceUnreachable(_) => "source_unreachable",
            Self::StoreUnknownStatus => "store_unknown_status",
            Self::Injected => "injected",
            Self::Io(_) => "io",
            Self::SizeMismatch => "size_mismatch",
        }
    }
}

/// The lifecycle state of one chunk on this device (spec § 2.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChunkState {
    /// The file is not on this device.
    NotInstalled,
    /// A request exists; nothing transferred yet.
    Queued,
    /// The network is metered and no request for the chunk allows that yet.
    WaitingForUnmetered,
    /// Bytes are being transferred.
    Downloading {
        /// Bytes on disk.
        done: u64,
        /// File size.
        total: u64,
    },
    /// The file is complete and its hash is being checked.
    Verifying,
    /// Present on disk, not mounted. Mount checks size and full-file BLAKE3 against the manifest.
    Installed,
    /// Its assets are visible to the asset server.
    Mounted,
    /// The transfer is paused; partial data is kept.
    Paused,
    /// Installed for an older build; a request refetches it.
    NeedsUpdate,
    /// The last attempt failed.
    Failed(FailReason),
    /// The request was cancelled; partial data is kept until `release`.
    Cancelled,
}

impl ChunkState {
    /// The state name used by the agent surface (`"Downloading"`, `"Failed(hash_mismatch)"`, ...).
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::NotInstalled => "NotInstalled".into(),
            Self::Queued => "Queued".into(),
            Self::WaitingForUnmetered => "WaitingForUnmetered".into(),
            Self::Downloading { .. } => "Downloading".into(),
            Self::Verifying => "Verifying".into(),
            Self::Installed => "Installed".into(),
            Self::Mounted => "Mounted".into(),
            Self::Paused => "Paused".into(),
            Self::NeedsUpdate => "NeedsUpdate".into(),
            Self::Failed(reason) => format!("Failed({})", reason.code()),
            Self::Cancelled => "Cancelled".into(),
        }
    }

    /// Present on the device (installed or mounted).
    #[must_use]
    pub fn is_present(&self) -> bool {
        matches!(self, Self::Installed | Self::Mounted)
    }

    /// A transfer is pending or running for it.
    #[must_use]
    pub fn is_in_flight(&self) -> bool {
        matches!(
            self,
            Self::Queued
                | Self::WaitingForUnmetered
                | Self::Downloading { .. }
                | Self::Verifying
                | Self::Paused
        )
    }
}

/// Errors of the content manifest, the manager and the chunk cook. [`ContentError::reason`] is the
/// agent `data.reason` code (spec § 5).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContentError {
    /// No `content-manifest.ron`.
    #[error("no content manifest")]
    ManifestMissing,
    /// The manifest (or chunk map) is newer than this build reads.
    #[error("content manifest version {found} is newer than supported version {supported}")]
    ManifestVersion {
        /// Version in the file.
        found: u32,
        /// Newest version this build reads.
        supported: u32,
    },
    /// Unknown chunk name.
    #[error("chunk {0:?} not found")]
    ChunkNotFound(String),
    /// Unknown asset.
    #[error("asset {0:?} not found")]
    AssetNotFound(String),
    /// The chunk is not installed.
    #[error("chunk {0:?} is not installed")]
    ChunkNotInstalled(String),
    /// An installed pak's regular-file size or full BLAKE3 differs from its manifest entry.
    #[error("chunk {0:?} pak does not match its manifest size or BLAKE3")]
    ChunkIntegrityMismatch(String),
    /// The chunk is mounted or pinned by a load set.
    #[error("chunk {0:?} is in use")]
    ChunkInUse(String),
    /// The chunk was cooked against a base this build does not accept.
    #[error("chunk {chunk:?} was built against base {expected:?}, running base is {running:?}")]
    ChunkBaseMismatch {
        /// Chunk.
        chunk: String,
        /// Base build id the chunk names.
        expected: String,
        /// Running base build id.
        running: String,
    },
    /// Unknown request id.
    #[error("request {0} not found")]
    RequestNotFound(u64),
    /// The request already finished.
    #[error("request {0} already finished")]
    RequestFinished(u64),
    /// `resume` of a request that is not paused.
    #[error("request {0} is not paused")]
    RequestNotPaused(u64),
    /// The source cannot provide the chunk.
    #[error("source cannot reach chunk {0:?}")]
    SourceUnreachable(String),
    /// The source kind is not available on this build or platform.
    #[error("source {0:?} not supported")]
    SourceNotSupported(String),
    /// A requires cycle among chunks.
    #[error("chunk requirement cycle among {0:?}")]
    ChunkCycle(Vec<String>),
    /// A parameter is invalid.
    #[error("bad parameter: {0}")]
    BadParams(String),
    /// A file does not parse.
    #[error("{0}")]
    Format(String),
    /// A file system error.
    #[error("io: {0}")]
    Io(String),
    /// A fetched manifest's signature is malformed, by a key the game does not trust, or does not
    /// match the manifest (ADR-2710). The running manifest is kept.
    #[error("content manifest signature invalid: {0}")]
    SignatureInvalid(String),
    /// The game trusts signing keys, requires signatures, and the host has none (ADR-2710). The
    /// running manifest is kept.
    #[error("content manifest signature missing: {0}")]
    SignatureMissing(String),
    /// The chunk's file is not the manifest's length: it is not mounted (ADR-8810).
    #[error("chunk {chunk:?} file is {found} bytes, the manifest says {expected}")]
    ChunkSizeMismatch {
        /// Chunk.
        chunk: String,
        /// Length in the manifest.
        expected: u64,
        /// Length on disk.
        found: u64,
    },
}

impl ContentError {
    /// The agent `data.reason` code.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::ManifestMissing => "manifest_missing",
            Self::ManifestVersion { .. } => "manifest_version",
            Self::ChunkNotFound(_) => "chunk_not_found",
            Self::AssetNotFound(_) => "asset_not_found",
            Self::ChunkNotInstalled(_) => "chunk_not_installed",
            Self::ChunkInUse(_) => "chunk_in_use",
            Self::ChunkIntegrityMismatch(_) => "chunk_integrity_mismatch",
            Self::ChunkBaseMismatch { .. } => "chunk_base_mismatch",
            Self::RequestNotFound(_) => "request_not_found",
            Self::RequestFinished(_) => "request_finished",
            Self::RequestNotPaused(_) => "request_not_paused",
            Self::SourceUnreachable(_) => "source_unreachable",
            Self::SourceNotSupported(_) => "source_not_supported",
            Self::ChunkCycle(_) => "chunk_cycle",
            Self::BadParams(_) | Self::Format(_) => "bad_params",
            Self::Io(_) => "io",
            Self::SignatureInvalid(_) => "content_signature_invalid",
            Self::SignatureMissing(_) => "content_signature_missing",
            Self::ChunkSizeMismatch { .. } => "chunk_size_mismatch",
        }
    }
}
