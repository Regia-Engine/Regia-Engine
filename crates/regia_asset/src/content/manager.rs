// SPDX-License-Identifier: MPL-2.0
//! [`ContentManager`]: chunk states, requests, transfers, verification and mounting (spec 40 § 2.5).

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use parking_lot::Mutex;
use regia_app::prelude::*;

use super::host::resolve_base_url;
use super::http::{HttpClient, HttpSource, fetch_manifest_trusted};
use super::manifest::{
    BASE_CHUNK, ChunkEntry, ChunkMap, ContentManifest, ContentState, Delivery, InstalledChunk, hash_file,
};
use super::source::{ChunkSource, LocalSource, SourceStatus, Transfer, TransferStep};
use super::trust::{CONTENT_TRUST_FILE, ContentTrust, SignatureCheck};
use super::why::{CHUNK_WHY_FILE, ChunkWhy};
use super::{ChunkState, ContentError, FailReason};
use crate::id::AssetId;
use crate::server::AssetServer;

thread_local! {
    static CONTENT_STATE_KEY_ALLOCS: Cell<u64> = const { Cell::new(0) };
    static CONTENT_QUEUE_WALKS: Cell<u64> = const { Cell::new(0) };
}

fn note_content_state_key_alloc() {
    CONTENT_STATE_KEY_ALLOCS.with(|n| n.set(n.get().saturating_add(1)));
}

fn note_content_queue_walk() {
    CONTENT_QUEUE_WALKS.with(|n| n.set(n.get().saturating_add(1)));
}

/// Times a chunk-state map key was allocated on this thread since
/// [`reset_content_state_key_allocs`]. Updating a chunk that is already in the map does not count.
#[must_use]
pub fn content_state_key_allocs() -> u64 {
    CONTENT_STATE_KEY_ALLOCS.with(Cell::get)
}

/// Zeroes [`content_state_key_allocs`] on this thread.
pub fn reset_content_state_key_allocs() {
    CONTENT_STATE_KEY_ALLOCS.with(|n| n.set(0));
}

/// Times [`ContentManager::advance`] walked queued chunk states on this thread since
/// [`reset_content_queue_walks`]. An advance with nothing in flight does not count.
#[must_use]
pub fn content_queue_walks() -> u64 {
    CONTENT_QUEUE_WALKS.with(Cell::get)
}

/// Zeroes [`content_queue_walks`] on this thread.
pub fn reset_content_queue_walks() {
    CONTENT_QUEUE_WALKS.with(|n| n.set(0));
}

/// A request whose chunks are installed, mounted, failed, or cancelled.
fn request_is_finished(request: &Request, states: &BTreeMap<String, ChunkState>) -> bool {
    request.cancelled
        || request.chunks.iter().all(|chunk| {
            matches!(
                states.get(chunk),
                Some(ChunkState::Installed | ChunkState::Mounted | ChunkState::Failed(_))
            ) && (!request.mount_when_done
                || request.unmounted.contains(chunk)
                || !matches!(states.get(chunk), Some(ChunkState::Installed)))
        })
}

/// Whether the agent label changed. `Downloading` with a new byte count is the same label.
fn state_label_changed(from: &ChunkState, to: &ChunkState) -> bool {
    match (from, to) {
        (ChunkState::Failed(left), ChunkState::Failed(right)) => left.code() != right.code(),
        _ => std::mem::discriminant(from) != std::mem::discriminant(to),
    }
}

/// Request priority tier: higher tiers are served first, then request order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    /// Served last.
    Low,
    /// Default.
    #[default]
    Normal,
    /// Served first.
    High,
}

impl Tier {
    /// Parses `low`, `normal` or `high`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "low" => Some(Self::Low),
            "normal" => Some(Self::Normal),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

/// Options of [`ContentManager::request`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequestOptions {
    /// Queue tier.
    pub priority: Tier,
    /// Transfer on a metered network without asking.
    pub allow_metered: bool,
    /// Mount each chunk as soon as it is verified (needs the asset server in the step).
    pub mount_when_done: bool,
}

/// A request handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub u64);

/// The progress of a request: summed bytes over every chunk it covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestProgress {
    /// Overall state label (`Queued`, `Downloading`, `Installed`, `Mounted`, `Paused`, `Cancelled`,
    /// `WaitingForUnmetered`, `Failed(<reason>)`).
    pub state: String,
    /// Bytes present.
    pub done: u64,
    /// Bytes of every chunk of the request.
    pub total: u64,
    /// `(chunk, state, done, total)` in request order (requirements first).
    pub chunks: Vec<(String, ChunkState, u64, u64)>,
}

/// Written whenever a chunk's state label changes (progress inside `Downloading` is not an event).
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct ChunkStateChanged {
    /// Chunk name.
    pub chunk: String,
    /// Previous state.
    pub from: ChunkState,
    /// New state.
    pub to: ChunkState,
}

/// A deterministic fault armed for one chunk (`content.inject`, debug builds and tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentFault {
    /// The transfer fails once `after_bytes` are on disk.
    Fail,
    /// The transfer stops moving once `after_bytes` are on disk.
    Stall,
    /// The network reads as metered.
    Metered,
    /// One byte of the finished file is flipped before verification.
    Corrupt,
    /// The free-space check fails before any byte is written.
    NoSpace,
}

impl ContentFault {
    /// Parses `fail`, `stall`, `metered`, `corrupt`, `no_space`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "fail" => Some(Self::Fail),
            "stall" => Some(Self::Stall),
            "metered" => Some(Self::Metered),
            "corrupt" => Some(Self::Corrupt),
            "no_space" => Some(Self::NoSpace),
            _ => None,
        }
    }
}

/// Bytes free on the volume of a path; `None` when the platform cannot tell (the check is then
/// skipped). The default is `regia_platform::paths::free_space`.
pub type SpaceProbe = Arc<dyn Fn(&Path) -> Option<u64> + Send + Sync>;

/// Asks the platform whether the active network is metered: `Some(true)`, `Some(false)`, or `None`
/// when it cannot tell (read as unmetered, spec 40 § 2.5). Tests pass a fake.
pub type MeteredProbe = Arc<dyn Fn() -> Option<bool> + Send + Sync>;

/// Busy [`ContentManager::advance`] calls between two metered-network polls (a poll also runs on
/// every request). An idle manager never polls.
pub const METERED_POLL_TICKS: u32 = 30;

/// The platform's metered-network query (`regia_platform::network::active_network_metered`):
/// Android asks `ConnectivityManager`; other platforms cannot tell (`None`). Errors read as `None`.
#[must_use]
pub fn platform_metered_probe() -> MeteredProbe {
    Arc::new(|| match regia_platform::network::active_network_metered() {
        Ok(answer) => answer,
        Err(e) => {
            tracing::debug!(target: "regia_asset", "metered-network query failed: {e}");
            None
        }
    })
}

/// What the manager knows about the network cost ([`ContentManager::network`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetworkCost {
    /// Whether transfers treat the network as metered now.
    pub metered: bool,
    /// The platform's last answer (`None`: cannot tell, or never asked).
    pub platform: Option<bool>,
    /// A probe is installed and has answered at least once.
    pub probed: bool,
    /// An injected `metered` fault holds the network metered whatever the platform says.
    pub forced: bool,
}

/// Runs one hash job off the frame. The job reports back through the manager's next
/// [`ContentManager::advance`].
pub type VerifyRunner = Arc<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>;

/// A [`VerifyRunner`] on the engine's I/O pool.
#[must_use]
pub fn io_pool_verify_runner() -> VerifyRunner {
    Arc::new(|job| regia_core::tasks::spawn_io(async move { job() }).detach())
}

/// A finished hash job.
struct VerifyDone {
    chunk: String,
    token: u64,
    hash: std::io::Result<String>,
}

/// A hash job's answer, sent exactly once: with the hash when the job ran, or with an error from
/// `Drop` when the runner dropped the job unrun or it panicked, so the chunk fails instead of
/// staying `Verifying` and `wait_verifications` cannot block forever (audit concurrency-1103 L6).
struct VerifyReport {
    done: Option<VerifyDone>,
    tx: Sender<VerifyDone>,
}

impl VerifyReport {
    fn send(&mut self, hash: std::io::Result<String>) {
        if let Some(mut done) = self.done.take() {
            done.hash = hash;
            // The manager may be gone: nobody is waiting then.
            let _ = self.tx.send(done);
        }
    }
}

impl Drop for VerifyReport {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            let _ = self.tx.send(done);
        }
    }
}

/// Where the manager reads and writes chunks.
#[derive(Clone, Debug, Resource)]
pub struct ContentSettings {
    /// The package's content directory (manifest, chunk map, install-time paks). Read-only.
    pub content_dir: PathBuf,
    /// Where fetched chunks and `content-state.ron` go (the app's private data directory).
    pub install_dir: PathBuf,
    /// Most chunk transfers running at once.
    pub max_concurrent: usize,
    /// Bytes each stepped transfer may move per frame.
    pub step_budget: u64,
    /// Mount every installed chunk at startup, in mount order.
    pub mount_installed_at_startup: bool,
    /// Hash finished transfers on the I/O pool instead of inside the frame (spec § 10).
    pub background_verify: bool,
    /// Where `external` chunks come from (`<base_url>/<build_id>/<file>`): `None` adds no source.
    /// From [`CONTENT_BASE_URL_ENV`](super::CONTENT_BASE_URL_ENV) or `content-host.ron`.
    pub base_url: Option<String>,
}

impl ContentSettings {
    /// Settings for `content_dir`, installing into `<local data dir>/content` on desktop (or
    /// `<content_dir>/downloads` when there is no data directory). On Android `content_dir` is
    /// already in the app's internal files directory (`regia` extracts it there), so fetched chunks
    /// go to `<content_dir>/downloads`, never to shared storage.
    #[must_use]
    pub fn for_content_dir(content_dir: impl Into<PathBuf>) -> Self {
        let content_dir = content_dir.into();
        let base_url = resolve_base_url(&content_dir);
        #[cfg(not(target_os = "android"))]
        let data_dir = regia_platform::paths::local_data_dir();
        #[cfg(target_os = "android")]
        let data_dir: Option<PathBuf> = None;
        let install_dir = data_dir
            .map(|d| d.join("content"))
            .unwrap_or_else(|| content_dir.join("downloads"));
        Self {
            content_dir,
            install_dir,
            max_concurrent: 4,
            step_budget: 8 << 20,
            mount_installed_at_startup: true,
            background_verify: true,
            base_url,
        }
    }
}

#[derive(Clone, Debug)]
struct Request {
    chunks: Vec<String>,
    priority: Tier,
    allow_metered: bool,
    mount_when_done: bool,
    paused: bool,
    cancelled: bool,
    /// Chunks unmounted on purpose since this request mounted them: it must not mount them again.
    unmounted: BTreeSet<String>,
}

/// File-system work of a [`ContentManager`]'s chunk scans (ADR-8810).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContentScanCounts {
    /// `stat` calls (one per packaged chunk; two for a fetched one).
    pub stats: u64,
    /// Whole-file hashes (a fetched chunk the install record does not vouch for).
    pub hashes: u64,
}

/// The content manager resource: every chunk's state, requests, sources and transfers.
#[derive(Resource)]
pub struct ContentManager {
    manifest: ContentManifest,
    chunk_map: ChunkMap,
    content_dir: PathBuf,
    install_dir: PathBuf,
    states: BTreeMap<String, ChunkState>,
    /// Bytes on disk of chunks in flight (kept across pause/cancel).
    progress: BTreeMap<String, u64>,
    /// Where each present chunk's file is.
    paths: BTreeMap<String, PathBuf>,
    sources: Vec<Box<dyn ChunkSource>>,
    requests: BTreeMap<u64, Request>,
    /// Finished requests. [`ContentManager::progress`] still reads them. They are outside
    /// `requests`, so a later [`ContentManager::advance`] can return before it walks chunk states.
    done_requests: BTreeMap<u64, Request>,
    next_request: u64,
    transfers: BTreeMap<String, Box<dyn Transfer>>,
    max_concurrent: usize,
    metered: bool,
    /// The platform's metered-network query; `None`: [`ContentManager::set_metered`] alone decides.
    metered_probe: Option<MeteredProbe>,
    /// The probe's last answer.
    metered_platform: Option<bool>,
    /// The probe has answered at least once.
    metered_probed: bool,
    /// An injected `metered` fault: metered whatever the probe says.
    metered_forced: bool,
    /// Busy advances since the last probe poll.
    metered_tick: u32,
    faults: BTreeMap<String, (ContentFault, u64)>,
    record: ContentState,
    events: Vec<ChunkStateChanged>,
    /// The cook's reason table, read on first use (`None` = not read yet).
    why: Option<Option<ChunkWhy>>,
    space_probe: SpaceProbe,
    /// `None`: finished transfers hash inside [`Self::advance`] (deterministic stepping).
    verify_runner: Option<VerifyRunner>,
    /// Hash jobs running, by chunk. A result whose token is not here is stale.
    verifying: BTreeMap<String, u64>,
    /// Chunks being hashed where a source keeps them, with that path (no `.part` to install).
    in_place: BTreeMap<String, PathBuf>,
    next_verify: u64,
    verify_tx: Sender<VerifyDone>,
    verify_rx: Mutex<Receiver<VerifyDone>>,
    /// Keys that may sign a fetched manifest (`content-trust.ron`, ADR-2710); `None`: unsigned
    /// manifests are accepted (TLS only).
    trust: Option<ContentTrust>,
    /// Why `content-trust.ron` could not be read: every manifest update is then refused.
    trust_error: Option<String>,
    /// File-system work of the scans ([`Self::scan_counts`]).
    scan_counts: ContentScanCounts,
}

/// What [`ContentManager::update_from_host`] adopted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestUpdate {
    /// Build id of the manifest that was running.
    pub previous_build_id: String,
    /// Build id of the adopted manifest.
    pub build_id: String,
    /// The signature check of the adopted manifest.
    pub signature: SignatureCheck,
    /// Chunks that are new or whose hash changed.
    pub changed: Vec<String>,
    /// Chunks the adopted manifest no longer lists.
    pub removed: Vec<String>,
}

impl std::fmt::Debug for ContentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentManager")
            .field("build_id", &self.manifest.build_id)
            .field("states", &self.states)
            .field("requests", &self.requests.len())
            .finish_non_exhaustive()
    }
}

impl ContentManager {
    /// Opens the manifest in `settings.content_dir`, with the DLC entries next to it
    /// ([`ContentManifest::read_from_dir_with_dlc`]), and scans what is installed.
    ///
    /// # Errors
    /// [`ContentError::ManifestMissing`] without a manifest; a newer or broken manifest.
    pub fn open(settings: &ContentSettings) -> Result<Self, ContentError> {
        let _span = tracing::info_span!(target: "regia_asset", "content_mount", phase = "open").entered();
        // DLC entries next to the manifest join it (spec 40 § 2.4), with their own chunk maps.
        let manifest = ContentManifest::read_from_dir_with_dlc(&settings.content_dir)?
            .ok_or(ContentError::ManifestMissing)?;
        let chunk_map = ChunkMap::read_from_dir_with_dlc(&settings.content_dir, &manifest)?;
        let mut manager = Self::from_manifest(manifest, &settings.content_dir, &settings.install_dir)
            .with_chunk_map(chunk_map);
        manager.max_concurrent = settings.max_concurrent.max(1);
        if settings.background_verify {
            manager.verify_runner = Some(io_pool_verify_runner());
        }
        // Fail closed: a trust record that does not read refuses every manifest update.
        match ContentTrust::read_from_dir(&settings.content_dir) {
            Ok(trust) => manager.trust = trust,
            Err(e) => {
                tracing::error!(target: "regia_asset", "{CONTENT_TRUST_FILE} not read, manifest updates refused: {e}");
                manager.trust_error = Some(e.to_string());
            }
        }
        Ok(manager)
    }

    /// The keys that may sign a fetched manifest (`content-trust.ron` next to the packaged
    /// manifest, ADR-2710); `None` when the game trusts none (unsigned manifests are accepted).
    #[must_use]
    pub fn trust(&self) -> Option<&ContentTrust> {
        self.trust.as_ref()
    }

    /// Why `content-trust.ron` could not be read (every manifest update is refused then).
    #[must_use]
    pub fn trust_error(&self) -> Option<&str> {
        self.trust_error.as_deref()
    }

    /// Replaces the trusted keys (tests and tools; a game reads them from `content-trust.ron`).
    pub fn set_trust(&mut self, trust: Option<ContentTrust>) {
        self.trust = trust;
        self.trust_error = None;
    }

    /// Fetches the manifest `base_url` offers, checks its signature against [`Self::trust`]
    /// before parsing it, and adopts it ([`Self::adopt_manifest`]); the `http` sources are pointed
    /// at the new build. On any error the running manifest, states and sources are unchanged.
    ///
    /// It waits for the host (up to its timeouts): for tools, launchers and loading screens, not
    /// for a frame.
    ///
    /// # Errors
    /// The errors of [`fetch_manifest_trusted`] (`content_signature_invalid`,
    /// `content_signature_missing`, `source_unreachable`, `chunk_base_mismatch`, ...), a trust
    /// record that did not read (`content_signature_invalid`), and those of
    /// [`Self::adopt_manifest`].
    pub fn update_from_host(
        &mut self,
        client: &HttpClient,
        base_url: &str,
    ) -> Result<ManifestUpdate, ContentError> {
        if let Some(why) = &self.trust_error {
            return Err(ContentError::SignatureInvalid(format!(
                "{CONTENT_TRUST_FILE} did not read ({why}); no manifest can be trusted"
            )));
        }
        let fetched = fetch_manifest_trusted(client, base_url, &self.manifest, self.trust.as_ref())?;
        let source = HttpSource::with_client(base_url, &fetched.manifest.build_id, client.clone())
            .map_err(ContentError::SourceNotSupported)?;
        let previous_build_id = self.manifest.build_id.clone();
        let build_id = fetched.manifest.build_id.clone();
        let (changed, removed) = self.adopt_manifest(fetched.manifest)?;
        // Every source for the old build's folder now points at the new one.
        let before = self.sources.len();
        self.sources.retain(|s| s.name() != "http");
        if self.sources.len() != before {
            self.sources.push(Box::new(source));
        }
        tracing::info!(target: "regia_asset", previous = %previous_build_id, build = %build_id, signature = fetched.signature.label(), "content manifest adopted");
        Ok(ManifestUpdate {
            previous_build_id,
            build_id,
            signature: fetched.signature,
            changed,
            removed,
        })
    }

    /// Replaces the running manifest with `manifest` (already verified by the caller) and rescans
    /// chunk states: a chunk whose hash changed reads `NeedsUpdate` (or `NotInstalled`) and a
    /// request fetches the new file. A changed packaged chunk whose file is the previous build's
    /// length reads `NeedsUpdate` too, not `Failed(size_mismatch)`; an unchanged entry whose file
    /// is the wrong length is still a torn install (ADR-8860). Mounted chunks with an unchanged
    /// hash stay mounted. Returns (chunks new or changed, chunks removed).
    ///
    /// # Errors
    /// [`ContentError::ChunkInUse`] while any chunk is in flight (queued,
    /// [`ChunkState::WaitingForUnmetered`], downloading, verifying, paused) or a mounted chunk
    /// would change or disappear: unmount it first. Nothing changes then.
    pub fn adopt_manifest(
        &mut self,
        manifest: ContentManifest,
    ) -> Result<(Vec<String>, Vec<String>), ContentError> {
        let _span = tracing::info_span!(target: "regia_asset", "content_mount", phase = "adopt").entered();
        if let Some(name) = self.transfers.keys().chain(self.verifying.keys()).next() {
            return Err(ContentError::ChunkInUse(name.clone()));
        }
        for (name, state) in &self.states {
            let same = || {
                manifest
                    .chunk(name)
                    .zip(self.manifest.chunk(name))
                    .is_some_and(|(new, old)| new.hash == old.hash && new.file == old.file)
            };
            if state.is_in_flight() || (*state == ChunkState::Mounted && !same()) {
                return Err(ContentError::ChunkInUse(name.clone()));
            }
        }
        let changed: Vec<String> = manifest
            .chunks
            .iter()
            .filter(|c| self.manifest.chunk(&c.name).is_none_or(|old| old.hash != c.hash))
            .map(|c| c.name.clone())
            .collect();
        let removed: Vec<String> = self
            .manifest
            .chunks
            .iter()
            .filter(|c| manifest.chunk(&c.name).is_none())
            .map(|c| c.name.clone())
            .collect();
        let old_states = std::mem::take(&mut self.states);
        let mounted: Vec<(String, Option<PathBuf>)> = old_states
            .iter()
            .filter(|(_, s)| **s == ChunkState::Mounted)
            .map(|(n, _)| (n.clone(), self.paths.get(n).cloned()))
            .collect();
        self.manifest = manifest;
        self.paths.clear();
        self.progress.clear();
        self.scan();
        // A packaged (install-time) file is only hashed at mount: one whose hash the new
        // manifest changed is stale, and only a fetched copy may replace it. An update usually
        // changes the length too, so the scan read the old packaged file as a torn install
        // (`Failed(size_mismatch)`): for a changed chunk that is the previous build, not damage
        // (ADR-8860).
        for name in &changed {
            let packaged = self.manifest.chunk(name).map(|c| self.content_dir.join(&c.file));
            let stale_packaged = packaged.is_some_and(|p| self.paths.get(name) == Some(&p));
            let resized = self.states.get(name) == Some(&ChunkState::Failed(FailReason::SizeMismatch));
            if stale_packaged {
                self.paths.remove(name);
            }
            if stale_packaged || resized {
                self.states.insert(name.clone(), ChunkState::NeedsUpdate);
            }
        }
        for (name, path) in mounted {
            if let Some(path) = path {
                self.paths.insert(name.clone(), path);
            }
            self.states.insert(name, ChunkState::Mounted);
        }
        let new_states = std::mem::take(&mut self.states);
        for (name, to) in new_states {
            let from = old_states.get(&name).cloned().unwrap_or(ChunkState::NotInstalled);
            self.states.insert(name.clone(), from);
            self.set_state(&name, to);
        }
        Ok((changed, removed))
    }

    /// A manager over `manifest`, reading install-time chunks from `content_dir` and fetched ones
    /// from `install_dir`. Scans both directories (hash-verifying fetched chunks the install record
    /// does not vouch for).
    #[must_use]
    pub fn from_manifest(manifest: ContentManifest, content_dir: &Path, install_dir: &Path) -> Self {
        let record = ContentState::read_from_dir(install_dir);
        let (verify_tx, verify_rx) = channel();
        let mut manager = Self {
            manifest,
            chunk_map: ChunkMap::default(),
            content_dir: content_dir.to_path_buf(),
            install_dir: install_dir.to_path_buf(),
            states: BTreeMap::new(),
            progress: BTreeMap::new(),
            paths: BTreeMap::new(),
            sources: vec![Box::new(LocalSource::new(content_dir))],
            requests: BTreeMap::new(),
            done_requests: BTreeMap::new(),
            next_request: 1,
            transfers: BTreeMap::new(),
            max_concurrent: 4,
            metered: false,
            metered_probe: None,
            metered_platform: None,
            metered_probed: false,
            metered_forced: false,
            metered_tick: 0,
            faults: BTreeMap::new(),
            record,
            events: Vec::new(),
            why: None,
            space_probe: Arc::new(|path| regia_platform::paths::free_space(path).ok()),
            verify_runner: None,
            verifying: BTreeMap::new(),
            in_place: BTreeMap::new(),
            next_verify: 0,
            verify_tx,
            verify_rx: Mutex::new(verify_rx),
            trust: None,
            trust_error: None,
            scan_counts: ContentScanCounts::default(),
        };
        manager.scan();
        manager
    }

    /// File-system work of this manager's scans so far (startup and manifest updates): the
    /// second launch of an unchanged install hashes nothing and stats each packaged pak once
    /// (ADR-8810).
    #[must_use]
    pub fn scan_counts(&self) -> ContentScanCounts {
        self.scan_counts
    }

    /// Replaces the chunk map (`chunk_of`).
    #[must_use]
    pub fn with_chunk_map(mut self, map: ChunkMap) -> Self {
        self.chunk_map = map;
        self
    }

    /// Finds every chunk's file. A packaged file costs one stat and is trusted by its length (a
    /// length that is not the manifest's `size` is `Failed(size_mismatch)` and never mounted,
    /// ADR-8810); a fetched one costs a stat for the packaged miss and one for itself, and is
    /// hashed only when the install record does not vouch for its `(size, mtime)`.
    fn scan(&mut self) {
        let chunks = self.manifest.chunks.clone();
        let mut record_changed = false;
        for chunk in &chunks {
            let packaged = self.content_dir.join(&chunk.file);
            let fetched = self.install_dir.join(&chunk.file);
            self.scan_counts.stats += 1;
            let packaged_len = std::fs::metadata(&packaged)
                .ok()
                .filter(std::fs::Metadata::is_file)
                .map(|m| m.len());
            if packaged_len == Some(chunk.size) {
                self.paths.insert(chunk.name.clone(), packaged);
                self.states.insert(chunk.name.clone(), ChunkState::Installed);
                continue;
            }
            if let Some(found) = packaged_len {
                tracing::warn!(
                    target: "regia_asset",
                    chunk = %chunk.name,
                    expected = chunk.size,
                    found,
                    "packaged chunk is not the manifest's length; not mounted (size_mismatch)"
                );
            }
            self.scan_counts.stats += 1;
            let fetched_meta = std::fs::metadata(&fetched)
                .ok()
                .filter(std::fs::Metadata::is_file);
            let state = if let Some(meta) = fetched_meta {
                match self.record.chunks.get(&chunk.name) {
                    Some(rec) if rec.hash != chunk.hash => ChunkState::NeedsUpdate,
                    Some(rec) if stamp_of(&meta) == (rec.size, rec.modified) && rec.size == chunk.size => {
                        self.paths.insert(chunk.name.clone(), fetched);
                        ChunkState::Installed
                    }
                    _ => {
                        self.scan_counts.hashes += 1;
                        match hash_file(&fetched) {
                            Ok(hash) if hash == chunk.hash => {
                                record_changed |= self.note_installed(chunk, &fetched);
                                self.paths.insert(chunk.name.clone(), fetched);
                                ChunkState::Installed
                            }
                            _ => ChunkState::NeedsUpdate,
                        }
                    }
                }
            } else {
                ChunkState::NotInstalled
            };
            // A torn packaged copy with no good fetched one says why it is not there.
            let state = if packaged_len.is_some() && !state.is_present() {
                ChunkState::Failed(FailReason::SizeMismatch)
            } else {
                state
            };
            self.states.insert(chunk.name.clone(), state);
        }
        if record_changed {
            self.save_record();
        }
    }

    fn note_installed(&mut self, chunk: &ChunkEntry, path: &Path) -> bool {
        let (size, modified) = file_stamp(path).unwrap_or((chunk.size, 0));
        let entry = InstalledChunk {
            build_id: self.manifest.build_id.clone(),
            hash: chunk.hash.clone(),
            size,
            modified,
        };
        self.record.chunks.insert(chunk.name.clone(), entry.clone()) != Some(entry)
    }

    fn save_record(&self) {
        if let Err(e) = self.record.write_to_dir(&self.install_dir) {
            tracing::warn!(target: "regia_asset", "content-state.ron not written: {e}");
        }
    }

    /// The manifest.
    #[must_use]
    pub fn manifest(&self) -> &ContentManifest {
        &self.manifest
    }

    /// The manifest, to change in place (tests and tools).
    pub fn manifest_mut(&mut self) -> &mut ContentManifest {
        &mut self.manifest
    }

    /// The chunk map.
    #[must_use]
    pub fn chunk_map(&self) -> &ChunkMap {
        &self.chunk_map
    }

    /// The install directory.
    #[must_use]
    pub fn install_dir(&self) -> &Path {
        &self.install_dir
    }

    /// The state of `chunk`.
    #[must_use]
    pub fn state(&self, chunk: &str) -> Option<ChunkState> {
        self.states.get(chunk).cloned()
    }

    /// The cook's reason table (`chunk-why.bin` in the content directory), read on the first call;
    /// `None` when the build has none (cooked before ADR-1876) or it does not read.
    pub fn why_table(&mut self) -> Option<&ChunkWhy> {
        if self.why.is_none() {
            let table = match ChunkWhy::read_from_dir(&self.content_dir) {
                Ok(table) => table,
                Err(e) => {
                    tracing::warn!(target: "regia_asset", "content: {CHUNK_WHY_FILE} does not read: {e}");
                    None
                }
            };
            self.why = Some(table);
        }
        self.why.as_ref().and_then(Option::as_ref)
    }

    /// Replaces the reason table (tests and tools).
    #[must_use]
    pub fn with_why_table(mut self, table: ChunkWhy) -> Self {
        self.why = Some(Some(table));
        self
    }

    /// The chunk that holds `id` (from the chunk map).
    #[must_use]
    pub fn chunk_of(&self, id: AssetId) -> Option<&str> {
        self.chunk_map.chunk_of(id)
    }

    /// The file of a present chunk.
    #[must_use]
    pub fn chunk_path(&self, chunk: &str) -> Option<&Path> {
        self.paths.get(chunk).map(PathBuf::as_path)
    }

    /// Adds a source; sources added later are asked first (the local content directory last).
    pub fn add_source(&mut self, source: Box<dyn ChunkSource>) {
        self.sources.push(source);
    }

    /// Removes every source but the local content directory.
    pub fn clear_sources(&mut self) {
        self.sources.truncate(1);
    }

    /// Names of the sources, most preferred first.
    #[must_use]
    pub fn source_names(&self) -> Vec<&'static str> {
        self.sources.iter().rev().map(|s| s.name()).collect()
    }

    /// Whether the network is metered (desktop default `false`). With a probe installed
    /// ([`ContentManager::set_metered_probe`]) the probe's next answer replaces this value; an
    /// injected `metered` fault is cleared by `set_metered(false)`.
    pub fn set_metered(&mut self, metered: bool) {
        self.metered = metered;
        if !metered {
            self.metered_forced = false;
        }
    }

    /// Installs (or removes) the platform's metered-network query and asks it at once. The
    /// [`ContentPlugin`] installs [`platform_metered_probe`]; tests install a fake. The probe is
    /// asked on every request and every [`METERED_POLL_TICKS`] busy advances, never when idle.
    pub fn set_metered_probe(&mut self, probe: Option<MeteredProbe>) {
        self.metered_probe = probe;
        self.metered_probed = false;
        self.metered_platform = None;
        self.refresh_metered();
    }

    /// Asks the probe now (no-op without one): the network is metered when the platform says so
    /// or an injected fault holds it; "cannot tell" reads as unmetered (spec 40 § 2.5). Requests
    /// waiting for an unmetered network are released by the next advance.
    pub fn refresh_metered(&mut self) {
        self.metered_tick = 0;
        let Some(probe) = self.metered_probe.clone() else {
            return;
        };
        let answer = probe();
        let metered = self.metered_forced || answer == Some(true);
        if metered != self.metered || !self.metered_probed {
            tracing::info!(target: "regia_asset", platform = ?answer, metered, "content network cost");
        }
        self.metered_platform = answer;
        self.metered_probed = true;
        self.metered = metered;
    }

    /// What the manager knows about the network cost.
    #[must_use]
    pub fn network(&self) -> NetworkCost {
        NetworkCost {
            metered: self.metered,
            platform: self.metered_platform,
            probed: self.metered_probed,
            forced: self.metered_forced,
        }
    }

    /// Most chunk transfers running at once (at least 1).
    pub fn set_max_concurrent(&mut self, max: usize) {
        self.max_concurrent = max.max(1);
    }

    /// Bytes free on the install volume; `None` when the platform cannot tell.
    #[must_use]
    pub fn free_space(&self) -> Option<u64> {
        (self.space_probe)(&self.install_dir)
    }

    /// Replaces the free-space query (tests, a platform with its own quota).
    pub fn set_space_probe(&mut self, probe: SpaceProbe) {
        self.space_probe = probe;
    }

    /// Where finished transfers hash: `None` inside [`Self::advance`], else through `runner`.
    pub fn set_verify_runner(&mut self, runner: Option<VerifyRunner>) {
        self.verify_runner = runner;
    }

    /// Arms `fault` for `chunk`, effective once `after_bytes` are on disk.
    ///
    /// # Errors
    /// [`ContentError::ChunkNotFound`].
    pub fn inject(&mut self, chunk: &str, fault: ContentFault, after_bytes: u64) -> Result<(), ContentError> {
        if !self.states.contains_key(chunk) {
            return Err(ContentError::ChunkNotFound(chunk.to_owned()));
        }
        if fault == ContentFault::Metered {
            self.metered = true;
            self.metered_forced = true;
        }
        self.faults.insert(chunk.to_owned(), (fault, after_bytes));
        Ok(())
    }

    /// Blocks until every threaded transfer (HTTP) has finished, so the next
    /// [`ContentManager::advance`] sees its outcome. Tests and tools only: never call it per frame.
    pub fn wait_for_transfers(&mut self) {
        for transfer in self.transfers.values_mut() {
            transfer.wait();
        }
    }

    /// Takes the state-change events written since the last call.
    pub fn drain_events(&mut self) -> Vec<ChunkStateChanged> {
        std::mem::take(&mut self.events)
    }

    fn set_state(&mut self, chunk: &str, to: ChunkState) {
        // A progress write (Downloading to Downloading) must not allocate a new map key.
        // The map borrow ends before `emit_state`, which needs the manager.
        let changed = if let Some(slot) = self.states.get_mut(chunk) {
            if !state_label_changed(slot, &to) {
                *slot = to;
                return;
            }
            Some(std::mem::replace(slot, to.clone()))
        } else {
            None
        };
        if let Some(from) = changed {
            self.emit_state(chunk, from, to);
            return;
        }
        note_content_state_key_alloc();
        self.states.insert(chunk.to_owned(), to.clone());
        let from = ChunkState::NotInstalled;
        if state_label_changed(&from, &to) {
            self.emit_state(chunk, from, to);
        }
    }

    fn emit_state(&mut self, chunk: &str, from: ChunkState, to: ChunkState) {
        // Info, not debug: a store build has no agent, and this line in logcat is how a store-track
        // check sees a pack arrive (ADR-7696). Only label changes reach here, never byte progress.
        tracing::info!(target: "regia_asset", chunk, from = %from.label(), to = %to.label(), "chunk state");
        self.events.push(ChunkStateChanged {
            chunk: chunk.to_owned(),
            from,
            to,
        });
    }

    fn entry(&self, chunk: &str) -> Result<&ChunkEntry, ContentError> {
        self.manifest
            .chunk(chunk)
            .ok_or_else(|| ContentError::ChunkNotFound(chunk.to_owned()))
    }

    /// `chunk` with its requirements, requirements first, each once (base only when asked for).
    fn expand(
        &self,
        chunk: &str,
        out: &mut Vec<String>,
        visiting: &mut BTreeSet<String>,
    ) -> Result<(), ContentError> {
        if out.iter().any(|c| c == chunk) {
            return Ok(());
        }
        if !visiting.insert(chunk.to_owned()) {
            return Err(ContentError::ChunkCycle(visiting.iter().cloned().collect()));
        }
        let requires: Vec<String> = self.entry(chunk)?.required_chunks().map(str::to_owned).collect();
        for req in requires.iter().filter(|r| r.as_str() != BASE_CHUNK) {
            self.expand(req, out, visiting)?;
        }
        visiting.remove(chunk);
        out.push(chunk.to_owned());
        Ok(())
    }

    /// Requests `chunks` (their requirements first, repeated names collapsed). Chunks already
    /// present count as done; the others are queued.
    ///
    /// # Errors
    /// [`ContentError::ChunkNotFound`], [`ContentError::BadParams`] for an empty list,
    /// [`ContentError::ChunkCycle`].
    pub fn request(&mut self, chunks: &[&str], opts: RequestOptions) -> Result<RequestId, ContentError> {
        self.refresh_metered();
        self.request_inner(chunks, opts)
    }

    /// Requests every `fast_follow` chunk that is not installed (or needs an update), mounting
    /// each when it is done: what the store does right after install on Android (spec 40 § 2.1).
    /// `Ok(None)` when there is nothing to fetch. The content plugin calls it at startup off
    /// Android when the game has a base URL (audit v0.2.12: desktop fast-follow chunks were never
    /// fetched until the game asked), and on Android at every launch, where it follows, takes or
    /// resumes the store's own fast-follow download (ADR-7696).
    ///
    /// # Errors
    /// As [`ContentManager::request`].
    pub fn request_fast_follow(&mut self) -> Result<Option<RequestId>, ContentError> {
        let names: Vec<String> = self
            .manifest
            .chunks
            .iter()
            .filter(|c| c.delivery == Delivery::FastFollow)
            .filter(|c| {
                matches!(
                    self.states.get(&c.name),
                    None | Some(
                        ChunkState::NotInstalled
                            | ChunkState::NeedsUpdate
                            | ChunkState::Failed(_)
                            | ChunkState::Cancelled
                    )
                )
            })
            .map(|c| c.name.clone())
            .collect();
        if names.is_empty() {
            return Ok(None);
        }
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        self.request_inner(
            &refs,
            RequestOptions {
                mount_when_done: true,
                ..RequestOptions::default()
            },
        )
        .map(Some)
    }

    fn request_inner(&mut self, chunks: &[&str], opts: RequestOptions) -> Result<RequestId, ContentError> {
        if chunks.is_empty() {
            return Err(ContentError::BadParams("no chunks".into()));
        }
        let mut set = Vec::new();
        for chunk in chunks {
            self.expand(chunk, &mut set, &mut BTreeSet::new())?;
        }
        for chunk in &set {
            let state = self
                .states
                .get(chunk)
                .cloned()
                .unwrap_or(ChunkState::NotInstalled);
            match state {
                ChunkState::NotInstalled
                | ChunkState::Cancelled
                | ChunkState::Failed(_)
                | ChunkState::NeedsUpdate => {
                    self.set_state(chunk, ChunkState::Queued);
                }
                _ => {}
            }
        }
        let id = self.next_request;
        self.next_request += 1;
        self.requests.insert(
            id,
            Request {
                chunks: set,
                priority: opts.priority,
                allow_metered: opts.allow_metered,
                mount_when_done: opts.mount_when_done,
                paused: false,
                cancelled: false,
                unmounted: BTreeSet::new(),
            },
        );
        Ok(RequestId(id))
    }

    fn chunk_bytes(&self, chunk: &str) -> (u64, u64) {
        let total = self.manifest.chunk(chunk).map_or(0, |c| c.size);
        let done = match self.states.get(chunk) {
            Some(ChunkState::Installed | ChunkState::Mounted | ChunkState::Verifying) => total,
            Some(ChunkState::Downloading { done, .. }) => *done,
            _ => self.progress.get(chunk).copied().unwrap_or(0),
        };
        (done.min(total), total)
    }

    /// Bytes still to fetch for a request (0 when everything is present).
    #[must_use]
    pub fn total_bytes(&self, id: RequestId) -> u64 {
        self.stored_request(id.0)
            .map_or(0, |r| r.chunks.iter().map(|c| self.chunk_bytes(c).1).sum())
    }

    /// The progress of a request.
    #[must_use]
    pub fn progress(&self, id: RequestId) -> Option<RequestProgress> {
        let request = self.stored_request(id.0)?;
        let mut chunks = Vec::with_capacity(request.chunks.len());
        let (mut done, mut total) = (0, 0);
        for name in &request.chunks {
            let (d, t) = self.chunk_bytes(name);
            done += d;
            total += t;
            let state = self.states.get(name).cloned().unwrap_or(ChunkState::NotInstalled);
            chunks.push((name.clone(), state, d, t));
        }
        let state = self.request_state(request);
        Some(RequestProgress {
            state,
            done,
            total,
            chunks,
        })
    }

    /// Every live and finished request with its progress, in request-id order: the
    /// `Content` predicate snapshot of `play.wait_until` / `play.expect`
    /// (`docs/specs/40` § 5's recipe step 3 waits on `requests.0.state`).
    #[must_use]
    pub fn request_snapshots(&self) -> Vec<(u64, RequestProgress)> {
        let mut out: Vec<(u64, RequestProgress)> = self
            .requests
            .keys()
            .chain(self.done_requests.keys())
            .filter_map(|id| self.progress(RequestId(*id)).map(|p| (*id, p)))
            .collect();
        out.sort_by_key(|(id, _)| *id);
        out
    }

    fn request_state(&self, request: &Request) -> String {
        let states: Vec<ChunkState> = request
            .chunks
            .iter()
            .map(|c| self.states.get(c).cloned().unwrap_or(ChunkState::NotInstalled))
            .collect();
        if let Some(ChunkState::Failed(reason)) = states.iter().find(|s| matches!(s, ChunkState::Failed(_))) {
            return format!("Failed({})", reason.code());
        }
        if request.cancelled {
            return "Cancelled".into();
        }
        if states.iter().all(|s| *s == ChunkState::Mounted) {
            return "Mounted".into();
        }
        if states.iter().all(ChunkState::is_present) {
            return "Installed".into();
        }
        if request.paused {
            return "Paused".into();
        }
        if states.contains(&ChunkState::WaitingForUnmetered) {
            return "WaitingForUnmetered".into();
        }
        if states
            .iter()
            .any(|s| matches!(s, ChunkState::Downloading { .. } | ChunkState::Verifying))
        {
            return "Downloading".into();
        }
        "Queued".into()
    }

    fn finished(&self, request: &Request) -> bool {
        request_is_finished(request, &self.states)
    }

    fn stored_request(&self, id: u64) -> Option<&Request> {
        self.requests.get(&id).or_else(|| self.done_requests.get(&id))
    }

    fn live_request(&self, id: RequestId) -> Result<&Request, ContentError> {
        if self.done_requests.contains_key(&id.0) {
            return Err(ContentError::RequestFinished(id.0));
        }
        let request = self
            .requests
            .get(&id.0)
            .ok_or(ContentError::RequestNotFound(id.0))?;
        if self.finished(request) {
            return Err(ContentError::RequestFinished(id.0));
        }
        Ok(request)
    }

    /// Moves finished requests out of `requests` so the next idle advance does not walk states.
    fn retire_finished_requests(&mut self) {
        let done: Vec<u64> = {
            let states = &self.states;
            self.requests
                .iter()
                .filter(|(_, request)| request_is_finished(request, states))
                .map(|(id, _)| *id)
                .collect()
        };
        for id in done {
            if let Some(request) = self.requests.remove(&id) {
                self.done_requests.insert(id, request);
            }
        }
    }

    /// Pauses a request: its running transfers stop (partial data kept).
    ///
    /// # Errors
    /// [`ContentError::RequestNotFound`], [`ContentError::RequestFinished`].
    pub fn pause(&mut self, id: RequestId) -> Result<String, ContentError> {
        let chunks = self.live_request(id)?.chunks.clone();
        if let Some(r) = self.requests.get_mut(&id.0) {
            r.paused = true;
        }
        for chunk in &chunks {
            if self.can_park(chunk) && !self.needed_unpaused(chunk, id) {
                self.park(chunk, ChunkState::Paused);
            }
        }
        Ok(self.progress(id).map_or_else(String::new, |p| p.state))
    }

    /// Resumes a paused request.
    ///
    /// # Errors
    /// [`ContentError::RequestNotFound`], [`ContentError::RequestFinished`],
    /// [`ContentError::RequestNotPaused`].
    pub fn resume(&mut self, id: RequestId) -> Result<String, ContentError> {
        let request = self.live_request(id)?;
        if !request.paused {
            return Err(ContentError::RequestNotPaused(id.0));
        }
        let chunks = request.chunks.clone();
        if let Some(r) = self.requests.get_mut(&id.0) {
            r.paused = false;
        }
        for chunk in &chunks {
            if self.states.get(chunk) == Some(&ChunkState::Paused) {
                self.set_state(chunk, ChunkState::Queued);
            }
        }
        Ok(self.progress(id).map_or_else(String::new, |p| p.state))
    }

    /// Cancels a request. Partial data stays for a later request to resume from.
    ///
    /// # Errors
    /// [`ContentError::RequestNotFound`], [`ContentError::RequestFinished`].
    pub fn cancel(&mut self, id: RequestId) -> Result<String, ContentError> {
        let chunks = self.live_request(id)?.chunks.clone();
        if let Some(r) = self.requests.get_mut(&id.0) {
            r.cancelled = true;
        }
        for chunk in &chunks {
            if self.can_park(chunk) && !self.needed_unpaused(chunk, id) {
                self.park(chunk, ChunkState::Cancelled);
            }
        }
        Ok("Cancelled".into())
    }

    /// Another live, unpaused request needs `chunk`.
    fn needed_unpaused(&self, chunk: &str, except: RequestId) -> bool {
        self.requests.iter().any(|(id, r)| {
            *id != except.0 && !r.paused && !r.cancelled && r.chunks.iter().any(|c| c == chunk)
        })
    }

    /// In flight and not being verified: a complete file finishes its hash job and installs.
    fn can_park(&self, chunk: &str) -> bool {
        self.states
            .get(chunk)
            .is_some_and(|s| s.is_in_flight() && *s != ChunkState::Verifying)
    }

    /// Stops a chunk's transfer, keeping its `.part` bytes.
    fn park(&mut self, chunk: &str, to: ChunkState) {
        if let Some(ChunkState::Downloading { done, .. }) = self.states.get(chunk) {
            self.progress.insert(chunk.to_owned(), *done);
        }
        self.transfers.remove(chunk);
        self.set_state(chunk, to);
    }

    /// Lets one request transfer on a metered network.
    ///
    /// # Errors
    /// [`ContentError::RequestNotFound`], [`ContentError::RequestFinished`].
    pub fn approve_metered(&mut self, id: RequestId) -> Result<String, ContentError> {
        self.live_request(id)?;
        if let Some(r) = self.requests.get_mut(&id.0) {
            r.allow_metered = true;
        }
        self.release_metered_waits();
        Ok(self.progress(id).map_or_else(String::new, |p| p.state))
    }

    /// Lets every waiting request transfer on a metered network.
    pub fn approve_all_metered(&mut self) {
        for r in self.requests.values_mut() {
            r.allow_metered = true;
        }
        self.release_metered_waits();
    }

    fn release_metered_waits(&mut self) {
        let waiting: Vec<String> = self
            .states
            .iter()
            .filter(|(_, s)| **s == ChunkState::WaitingForUnmetered)
            .map(|(c, _)| c.clone())
            .collect();
        for chunk in waiting {
            if !self.metered || self.metered_allowed(&chunk) {
                self.set_state(&chunk, ChunkState::Queued);
            }
        }
    }

    fn metered_allowed(&self, chunk: &str) -> bool {
        self.requests
            .values()
            .any(|r| r.allow_metered && !r.cancelled && r.chunks.iter().any(|c| c == chunk))
    }

    /// The queue key of a queued chunk: best tier among its live requests, then the oldest of those.
    fn queue_key(&self, chunk: &str) -> Option<(std::cmp::Reverse<Tier>, u64, usize)> {
        self.requests
            .iter()
            .filter(|(_, r)| !r.paused && !r.cancelled)
            .filter_map(|(id, r)| {
                r.chunks
                    .iter()
                    .position(|c| c == chunk)
                    .map(|pos| (std::cmp::Reverse(r.priority), *id, pos))
            })
            .min()
    }

    fn wants_mount(&self, chunk: &str) -> bool {
        self.requests.values().any(|r| {
            r.mount_when_done
                && !r.cancelled
                && !r.unmounted.contains(chunk)
                && r.chunks.iter().any(|c| c == chunk)
        })
    }

    fn part_path(&self, entry: &ChunkEntry) -> PathBuf {
        self.install_dir.join(format!("{}.part", entry.file))
    }

    /// One step: starts queued chunks (tier, then request order, at most `max_concurrent`
    /// running), moves every running transfer by up to `budget` bytes, verifies finished files and
    /// mounts them when a request asked for it and `server` is given.
    pub fn advance(&mut self, budget: u64, mut server: Option<&mut AssetServer>) {
        let _span = tracing::debug_span!(target: "regia_asset", "content_tick").entered();
        self.retire_finished_requests();
        if self.requests.is_empty() && self.transfers.is_empty() && self.verifying.is_empty() {
            return;
        }
        self.poll_verifications(server.as_deref_mut());
        self.metered_tick = self.metered_tick.saturating_add(1);
        if self.metered_probe.is_some() && self.metered_tick >= METERED_POLL_TICKS {
            self.refresh_metered();
        }
        self.release_metered_waits();
        self.start_queued();
        let running: Vec<String> = self.transfers.keys().cloned().collect();
        for chunk in running {
            self.step_transfer(&chunk, budget, server.as_deref_mut());
        }
        // Chunks that were already present when requested with `mount_when_done`.
        if let Some(server) = server {
            let to_mount: Vec<String> = self
                .states
                .iter()
                .filter(|(c, s)| **s == ChunkState::Installed && self.wants_mount(c))
                .map(|(c, _)| c.clone())
                .collect();
            for chunk in to_mount {
                if let Err(e) = self.mount(server, &chunk) {
                    tracing::warn!(target: "regia_asset", chunk, "chunk not mounted: {e}");
                }
            }
        }
        self.retire_finished_requests();
    }

    fn start_queued(&mut self) {
        note_content_queue_walk();
        let mut queued: Vec<(std::cmp::Reverse<Tier>, u64, usize, String)> = self
            .states
            .iter()
            .filter(|(_, s)| **s == ChunkState::Queued)
            .filter_map(|(c, _)| self.queue_key(c).map(|(t, id, pos)| (t, id, pos, c.clone())))
            .collect();
        queued.sort();
        for (_, _, _, chunk) in queued {
            if self.metered && !self.metered_allowed(&chunk) {
                self.set_state(&chunk, ChunkState::WaitingForUnmetered);
                continue;
            }
            if self.transfers.len() >= self.max_concurrent {
                break;
            }
            if self.entry(&chunk).is_err() {
                continue;
            }
            if let Some((ContentFault::NoSpace, _)) = self.faults.get(&chunk) {
                self.faults.remove(&chunk);
                self.set_state(&chunk, ChunkState::Failed(FailReason::NoSpace));
                continue;
            }
            let mut started = false;
            let source_count = self.sources.len();
            for index in (0..source_count).rev() {
                let Some(status) = self.source_status(index, &chunk) else {
                    continue;
                };
                match status {
                    SourceStatus::Present(path) => {
                        if self.sources[index].verify_present() {
                            // One clone for the hash, not one per later step.
                            let Ok(entry) = self.entry(&chunk).cloned() else {
                                continue;
                            };
                            if !self.verified_before(&entry, &path) {
                                self.in_place.insert(chunk.clone(), path);
                                self.set_state(&chunk, ChunkState::Verifying);
                                self.begin_verify(&entry, None);
                                started = true;
                                break;
                            }
                        }
                        self.paths.insert(chunk.clone(), path);
                        self.set_state(&chunk, ChunkState::Installed);
                        started = true;
                        break;
                    }
                    SourceStatus::Available => {
                        let Some((size, part)) = self.chunk_part(&chunk) else {
                            continue;
                        };
                        let done = std::fs::metadata(&part).map_or(0, |m| m.len()).min(size);
                        if self.sources[index].uses_install_space() && !self.has_space(size - done) {
                            self.set_state(&chunk, ChunkState::Failed(FailReason::NoSpace));
                            started = true;
                            break;
                        }
                        let Some(transfer) = self.start_source(index, &chunk, &part) else {
                            continue;
                        };
                        self.transfers.insert(chunk.clone(), transfer);
                        self.set_state(&chunk, ChunkState::Downloading { done, total: size });
                        started = true;
                        break;
                    }
                    SourceStatus::Missing => {}
                }
            }
            if !started {
                let file = self
                    .manifest
                    .chunk(&chunk)
                    .map(|entry| entry.file.clone())
                    .unwrap_or_default();
                self.set_state(
                    &chunk,
                    ChunkState::Failed(FailReason::SourceUnreachable(format!("no source has {file}"))),
                );
            }
        }
    }

    fn source_status(&self, index: usize, chunk: &str) -> Option<SourceStatus> {
        let entry = self.manifest.chunk(chunk)?;
        Some(self.sources[index].status(entry))
    }

    fn chunk_part(&self, chunk: &str) -> Option<(u64, PathBuf)> {
        let entry = self.manifest.chunk(chunk)?;
        let part = self.install_dir.join(format!("{}.part", entry.file));
        Some((entry.size, part))
    }

    /// Starts a transfer without cloning the chunk record. `blocks` is copied by the source.
    fn start_source(&mut self, index: usize, chunk: &str, dest: &Path) -> Option<Box<dyn Transfer>> {
        let ContentManager {
            sources, manifest, ..
        } = self;
        let entry = manifest.chunk(chunk)?;
        Some(sources[index].start(entry, dest))
    }

    /// Whether `path` is the file this chunk's hash was last checked on: same hash, same stamp.
    fn verified_before(&self, entry: &ChunkEntry, path: &Path) -> bool {
        self.record
            .chunks
            .get(&entry.name)
            .is_some_and(|rec| rec.hash == entry.hash && file_stamp(path) == Some((rec.size, rec.modified)))
    }

    /// Whether the install volume holds `missing` bytes plus 5 percent (spec § 2.5) on top of what
    /// the running transfers still have to write. A running store download is counted too: its
    /// pack may have to be copied in (see `store`), and a copy that does not fit fails the chunk.
    fn has_space(&self, missing: u64) -> bool {
        let Some(free) = self.free_space() else {
            return true;
        };
        let running: u64 = self
            .transfers
            .keys()
            .map(|c| self.chunk_bytes(c))
            .map(|(done, total)| total - done)
            .sum();
        let wanted = missing
            .saturating_add(missing.div_ceil(20))
            .saturating_add(running);
        if free < wanted {
            tracing::warn!(target: "regia_asset", free, wanted, "not enough space for a chunk");
            return false;
        }
        true
    }

    fn step_transfer(&mut self, chunk: &str, budget: u64, server: Option<&mut AssetServer>) {
        if self.entry(chunk).is_err() {
            self.transfers.remove(chunk);
            return;
        }
        let done_now = match self.states.get(chunk) {
            Some(ChunkState::Downloading { done, .. }) => *done,
            _ => 0,
        };
        let mut budget = budget;
        match self.faults.get(chunk) {
            Some((ContentFault::Stall, after)) if done_now >= *after => return,
            Some((ContentFault::Stall | ContentFault::Fail, after)) => {
                budget = budget.min(after.saturating_sub(done_now).max(1));
            }
            _ => {}
        }
        let step = {
            let Some(transfer) = self.transfers.get_mut(chunk) else {
                return;
            };
            transfer.step(budget)
        };
        match step {
            TransferStep::Progress { done, total } => {
                self.set_state(chunk, ChunkState::Downloading { done, total });
                if let Some((ContentFault::Fail, after)) = self.faults.get(chunk)
                    && done >= *after
                {
                    self.faults.remove(chunk);
                    self.park(chunk, ChunkState::Failed(FailReason::Injected));
                }
            }
            TransferStep::Failed(why) => {
                let reason = if why.starts_with(super::store::STORE_UNKNOWN_STATUS) {
                    FailReason::StoreUnknownStatus
                } else {
                    FailReason::SourceUnreachable(why)
                };
                self.park(chunk, ChunkState::Failed(reason));
            }
            TransferStep::Done => {
                self.transfers.remove(chunk);
                self.progress.remove(chunk);
                self.set_state(chunk, ChunkState::Verifying);
                // The record is cloned only when the step needs it. Progress steps do not.
                let Ok(entry) = self.entry(chunk).cloned() else {
                    return;
                };
                self.begin_verify(&entry, server);
            }
        }
    }

    /// Hashes a finished `.part` file, inline or through the runner.
    fn begin_verify(&mut self, entry: &ChunkEntry, server: Option<&mut AssetServer>) {
        let part = self
            .in_place
            .get(&entry.name)
            .cloned()
            .unwrap_or_else(|| self.part_path(entry));
        if let Some((ContentFault::Corrupt, _)) = self.faults.get(&entry.name) {
            self.faults.remove(&entry.name);
            if let Err(e) = flip_first_byte(&part) {
                tracing::warn!(target: "regia_asset", "corrupt fault not applied: {e}");
            }
        }
        let Some(runner) = &self.verify_runner else {
            let hash = hash_file(&part);
            self.finish_verify(entry, hash, server);
            return;
        };
        self.next_verify += 1;
        let token = self.next_verify;
        self.verifying.insert(entry.name.clone(), token);
        let report = VerifyReport {
            done: Some(VerifyDone {
                chunk: entry.name.clone(),
                token,
                hash: Err(std::io::Error::other("the hash job ended without a result")),
            }),
            tx: self.verify_tx.clone(),
        };
        runner(Box::new(move || {
            let mut report = report;
            let hash = hash_file(&part);
            report.send(hash);
        }));
    }

    /// Takes the hash jobs that finished; `release` makes a running job stale.
    fn poll_verifications(&mut self, mut server: Option<&mut AssetServer>) {
        if self.verifying.is_empty() {
            return;
        }
        loop {
            let done = self.verify_rx.lock().try_recv();
            let Ok(done) = done else {
                return;
            };
            self.accept_verification(done, server.as_deref_mut());
        }
    }

    fn accept_verification(&mut self, done: VerifyDone, server: Option<&mut AssetServer>) {
        if self.verifying.get(&done.chunk) != Some(&done.token) {
            return;
        }
        self.verifying.remove(&done.chunk);
        if let Ok(entry) = self.entry(&done.chunk).cloned() {
            self.finish_verify(&entry, done.hash, server);
        }
    }

    /// Blocks until every running hash job has reported, then installs (tests and tools; never
    /// per frame).
    pub fn wait_verifications(&mut self, mut server: Option<&mut AssetServer>) {
        while !self.verifying.is_empty() {
            let done = self.verify_rx.lock().recv();
            let Ok(done) = done else {
                return;
            };
            self.accept_verification(done, server.as_deref_mut());
        }
    }

    fn finish_verify(
        &mut self,
        entry: &ChunkEntry,
        hash: std::io::Result<String>,
        server: Option<&mut AssetServer>,
    ) {
        if let Some(path) = self.in_place.remove(&entry.name) {
            let state = match hash {
                Ok(hash) if hash == entry.hash => {
                    if self.note_installed(entry, &path) {
                        self.save_record();
                    }
                    self.paths.insert(entry.name.clone(), path);
                    ChunkState::Installed
                }
                Ok(_) => {
                    tracing::warn!(target: "regia_asset", chunk = %entry.name, "a source's chunk failed verification; dropped from the source");
                    // As a bad `.part` is deleted: the next request fetches it, not the same file.
                    for source in self.sources.iter().filter(|s| s.verify_present()) {
                        source.release(entry);
                    }
                    ChunkState::Failed(FailReason::HashMismatch)
                }
                Err(e) => ChunkState::Failed(FailReason::Io(e.to_string())),
            };
            self.set_state(&entry.name, state);
            return;
        }
        let part = self.part_path(entry);
        let hash = match hash {
            Ok(hash) => hash,
            Err(e) => {
                self.set_state(&entry.name, ChunkState::Failed(FailReason::Io(e.to_string())));
                return;
            }
        };
        if hash != entry.hash {
            let _ = std::fs::remove_file(&part);
            tracing::warn!(target: "regia_asset", chunk = %entry.name, "chunk failed verification; file deleted");
            self.set_state(&entry.name, ChunkState::Failed(FailReason::HashMismatch));
            return;
        }
        let dest = self.install_dir.join(&entry.file);
        if let Err(e) = std::fs::rename(&part, &dest) {
            self.set_state(&entry.name, ChunkState::Failed(FailReason::Io(e.to_string())));
            return;
        }
        if self.note_installed(entry, &dest) {
            self.save_record();
        }
        self.paths.insert(entry.name.clone(), dest);
        self.set_state(&entry.name, ChunkState::Installed);
        if let Some(server) = server
            && self.wants_mount(&entry.name)
            && let Err(e) = self.mount(server, &entry.name)
        {
            tracing::warn!(target: "regia_asset", chunk = %entry.name, "chunk not mounted: {e}");
        }
    }

    /// Mounts an installed chunk (and its installed requirements first) at its manifest position.
    ///
    /// # Errors
    /// [`ContentError::ChunkNotFound`], [`ContentError::ChunkNotInstalled`],
    /// [`ContentError::ChunkBaseMismatch`], [`ContentError::ChunkSizeMismatch`] when the installed
    /// pak is not its manifest length (checked first, ADR-8714), [`ContentError::ChunkIntegrityMismatch`]
    /// when its full BLAKE3 differs, or the pak does not open.
    pub fn mount(&mut self, server: &mut AssetServer, chunk: &str) -> Result<(), ContentError> {
        self.mount_visiting(server, chunk, &mut BTreeSet::new())
    }

    /// [`Self::mount`] with the chain above it: `request` refuses a `requires` cycle and the cook
    /// never writes one, but a startup mount walks a manifest with neither in front of it, and a
    /// cycle there ended in a stack overflow.
    fn mount_visiting(
        &mut self,
        server: &mut AssetServer,
        chunk: &str,
        visiting: &mut BTreeSet<String>,
    ) -> Result<(), ContentError> {
        let _span = tracing::info_span!(target: "regia_asset", "content_mount", chunk).entered();
        let entry = self.entry(chunk)?.clone();
        match self.states.get(chunk) {
            Some(ChunkState::Mounted) => return Ok(()),
            Some(ChunkState::Installed) => {}
            _ => return Err(ContentError::ChunkNotInstalled(chunk.to_owned())),
        }
        if !visiting.insert(chunk.to_owned()) {
            return Err(ContentError::ChunkCycle(visiting.iter().cloned().collect()));
        }
        if let Some(base) = entry.base_build()
            && !self.manifest.accepts_base(base)
        {
            return Err(ContentError::ChunkBaseMismatch {
                chunk: chunk.to_owned(),
                expected: base.to_owned(),
                running: self.manifest.build_id.clone(),
            });
        }
        let requires: Vec<String> = entry
            .required_chunks()
            .filter(|r| *r != chunk)
            .map(str::to_owned)
            .collect();
        for req in requires {
            if self.manifest.chunk(&req).is_some() {
                self.mount_visiting(server, &req, visiting)?;
            }
        }
        visiting.remove(chunk);
        let path = self
            .paths
            .get(chunk)
            .cloned()
            .ok_or_else(|| ContentError::ChunkNotInstalled(chunk.to_owned()))?;
        // The file may have changed since the scan: a length that is not the manifest's is never
        // mounted (ADR-8810; one stat per mount, not per frame).
        let found = std::fs::metadata(&path)
            .map_err(|e| ContentError::Io(format!("{}: {e}", path.display())))?
            .len();
        if found != entry.size {
            self.paths.remove(chunk);
            self.set_state(chunk, ChunkState::Failed(FailReason::SizeMismatch));
            return Err(ContentError::ChunkSizeMismatch {
                chunk: chunk.to_owned(),
                expected: entry.size,
                found,
            });
        }
        if let Err(error) = entry.verify_installed_pak(&path) {
            let reason = match &error {
                ContentError::ChunkIntegrityMismatch(_) => FailReason::HashMismatch,
                _ => FailReason::Io(error.to_string()),
            };
            self.set_state(chunk, ChunkState::Failed(reason));
            return Err(error);
        }
        server
            .mount_chunk_pak(&path, chunk, Some(entry.mount_order))
            .map_err(|e| ContentError::Io(format!("{}: {e}", path.display())))?;
        self.set_state(chunk, ChunkState::Mounted);
        Ok(())
    }

    /// Unmounts a chunk. Load sets that pin its assets make this [`ContentError::ChunkInUse`]
    /// unless `force`, which removes those sets first. Handles into the chunk then read as
    /// unavailable.
    ///
    /// # Errors
    /// [`ContentError::ChunkNotFound`], [`ContentError::ChunkInUse`] (also for `base`).
    pub fn unmount(
        &mut self,
        server: &mut AssetServer,
        sets: Option<&crate::LoadSets>,
        chunk: &str,
        force: bool,
    ) -> Result<(), ContentError> {
        self.entry(chunk)?;
        if chunk == BASE_CHUNK {
            return Err(ContentError::ChunkInUse(chunk.to_owned()));
        }
        if self.states.get(chunk) != Some(&ChunkState::Mounted) {
            return Ok(());
        }
        if let Some(sets) = sets {
            let pinning = self.pinning_sets(server, sets, chunk);
            if !pinning.is_empty() {
                if !force {
                    return Err(ContentError::ChunkInUse(chunk.to_owned()));
                }
                for name in &pinning {
                    sets.remove(name);
                }
            }
        }
        server.unmount_chunk_pak(chunk);
        self.set_state(chunk, ChunkState::Installed);
        for request in self.requests.values_mut() {
            if request.chunks.iter().any(|c| c == chunk) {
                request.unmounted.insert(chunk.to_owned());
            }
        }
        Ok(())
    }

    /// Load sets with an asset item that unmounting `chunk` would make unavailable.
    fn pinning_sets(&self, server: &AssetServer, sets: &crate::LoadSets, chunk: &str) -> Vec<String> {
        sets.names()
            .into_iter()
            .filter(|name| {
                sets.get(name).is_some_and(|set| {
                    set.items.iter().any(|item| {
                        item.kind.is_asset()
                            && server
                                .id_for_path(&item.key)
                                .or_else(|| item.key.parse::<AssetId>().ok())
                                .is_some_and(|id| {
                                    // A duplicated asset stays loadable while another mounted pak
                                    // holds it; the last pak that does pins it, home or not.
                                    let holds = |c: &str| c == chunk;
                                    !server.chunks_holding(id).any(|c| !holds(c))
                                        && (self.chunk_map.chunk_of(id) == Some(chunk)
                                            || server.chunks_holding(id).any(holds))
                                })
                    })
                })
            })
            .collect()
    }

    /// Deletes fetched chunk files (and partial data) of `chunks` except `keep`; they become
    /// `NotInstalled`. Returns the chunks released.
    ///
    /// # Errors
    /// [`ContentError::ChunkNotFound`]; [`ContentError::ChunkInUse`] for a mounted chunk, `base`,
    /// or a chunk that ships inside the package.
    pub fn release(&mut self, chunks: &[&str], keep: &[&str]) -> Result<Vec<String>, ContentError> {
        for chunk in chunks {
            self.entry(chunk)?;
            if keep.contains(chunk) {
                continue;
            }
            // On Android the install directory lies inside the content directory.
            let packaged = self
                .paths
                .get(*chunk)
                .is_some_and(|p| p.starts_with(&self.content_dir) && !p.starts_with(&self.install_dir));
            if *chunk == BASE_CHUNK || packaged || self.states.get(*chunk) == Some(&ChunkState::Mounted) {
                return Err(ContentError::ChunkInUse((*chunk).to_owned()));
            }
        }
        let mut released = Vec::new();
        for chunk in chunks.iter().filter(|c| !keep.contains(c)) {
            let Ok(entry) = self.entry(chunk).cloned() else {
                continue;
            };
            self.transfers.remove(*chunk);
            self.verifying.remove(*chunk);
            self.in_place.remove(*chunk);
            // A file that is still there after a failed delete means the chunk is not released.
            let mut stuck = None;
            for file in [self.part_path(&entry), self.install_dir.join(&entry.file)] {
                if let Err(error) = std::fs::remove_file(&file)
                    && error.kind() != std::io::ErrorKind::NotFound
                    && file.exists()
                {
                    tracing::warn!(target: "regia_asset", "cannot delete {}: {error}", file.display());
                    stuck = Some(format!("cannot delete {}: {error}", file.display()));
                }
            }
            if let Some(why) = stuck {
                // The transfer and the verify job are gone: an in-flight chunk left as it was could
                // never move again (audit concurrency-1103 M1). It fails, and a new request retries.
                if matches!(
                    self.states.get(*chunk),
                    Some(ChunkState::Downloading { .. } | ChunkState::Verifying)
                ) {
                    self.set_state(chunk, ChunkState::Failed(FailReason::Io(why)));
                }
                continue;
            }
            for source in &self.sources {
                source.release(&entry);
            }
            self.paths.remove(*chunk);
            self.progress.remove(*chunk);
            if self.record.chunks.remove(*chunk).is_some() {
                self.save_record();
            }
            self.set_state(chunk, ChunkState::NotInstalled);
            released.push((*chunk).to_owned());
            self.end_requests_listing(chunk, keep);
        }
        Ok(released)
    }

    /// Nothing restarts a released chunk, so a request that still lists it would stay `Queued`
    /// forever: it ends here. Its other chunks stop only when nothing else wants them: a chunk in
    /// `keep` or on a running request goes on, and one a paused request lists parks as `Paused`, so
    /// `resume` finds it. Partial data is kept.
    fn end_requests_listing(&mut self, chunk: &str, keep: &[&str]) {
        let mut others: Vec<String> = Vec::new();
        // A retired (finished) request listing the chunk ends too: its chunk is gone, and with
        // nothing to mark it a retired request read `Queued` forever.
        for r in self.requests.values_mut().chain(self.done_requests.values_mut()) {
            if !r.cancelled && r.chunks.iter().any(|c| c == chunk) {
                r.cancelled = true;
                others.extend(r.chunks.iter().cloned());
            }
        }
        for other in others {
            let live = |paused: bool| {
                self.requests
                    .values()
                    .any(|r| !r.cancelled && r.paused == paused && r.chunks.contains(&other))
            };
            let in_flight = self.states.get(&other).is_some_and(ChunkState::is_in_flight);
            if !in_flight || keep.contains(&other.as_str()) || live(false) {
                continue;
            }
            let parked = if live(true) {
                ChunkState::Paused
            } else {
                ChunkState::Cancelled
            };
            self.park(&other, parked);
        }
    }

    /// Marks chunks whose pak the asset server already mounted (startup) as `Mounted`, and mounts
    /// every other installed chunk when `mount_installed`.
    pub fn sync_with_server(&mut self, server: &mut AssetServer, mount_installed: bool) {
        let mounted: BTreeSet<String> = server.mounted_pak_names().into_iter().collect();
        let names: Vec<String> = self.manifest.chunks.iter().map(|c| c.name.clone()).collect();
        for name in &names {
            let stem = self.manifest.chunk(name).and_then(|c| {
                Path::new(&c.file)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
            });
            if stem.is_some_and(|s| mounted.contains(&s))
                && self.states.get(name) == Some(&ChunkState::Installed)
            {
                self.set_state(name, ChunkState::Mounted);
            }
        }
        if mount_installed {
            let mut installed: Vec<(u32, String)> = self
                .manifest
                .chunks
                .iter()
                .filter(|c| self.states.get(&c.name) == Some(&ChunkState::Installed))
                .map(|c| (c.mount_order, c.name.clone()))
                .collect();
            installed.sort();
            for (_, name) in installed {
                if let Err(e) = self.mount(server, &name) {
                    tracing::warn!(target: "regia_asset", chunk = %name, "installed chunk not mounted: {e}");
                }
            }
        }
        server.set_chunk_map(Some(self.chunk_map.clone()));
    }
}

fn file_stamp(path: &Path) -> Option<(u64, u64)> {
    std::fs::metadata(path).ok().map(|meta| stamp_of(&meta))
}

/// `(size, mtime seconds)` of a file's metadata, the install record's stamp.
fn stamp_of(meta: &std::fs::Metadata) -> (u64, u64) {
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    (meta.len(), modified)
}

fn flip_first_byte(path: &Path) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&[byte[0] ^ 0xff])?;
    file.sync_all()
}

/// The startup half of [`ContentPlugin`]: with a source that can fetch fast-follow chunks (a
/// content host, or the Android store), requests every one that is not installed, mounting each
/// when it is done. A store pack the store already finished is taken in place, one it is still
/// downloading is followed, and one the player or the network stopped is fetched again. Without
/// such a source it only warns when a fast-follow chunk is missing. Returns the request.
pub(crate) fn start_fast_follow(manager: &mut ContentManager, fetching_source: bool) -> Option<RequestId> {
    if fetching_source {
        match manager.request_fast_follow() {
            Ok(Some(id)) => {
                tracing::info!(target: "regia_asset", request = id.0, "fast-follow chunks requested");
                return Some(id);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(target: "regia_asset", "fast-follow chunks not requested: {e}"),
        }
    } else if manager.manifest().chunks.iter().any(|c| {
        c.delivery == Delivery::FastFollow && manager.state(&c.name) == Some(ChunkState::NotInstalled)
    }) {
        tracing::warn!(target: "regia_asset", "fast-follow chunks are not installed and the game has no usable content base URL: they load only after a request with a source");
    }
    None
}

/// Steps the manager once per frame and forwards its state-change events.
pub fn content_tick(
    manager: Option<ResMut<ContentManager>>,
    server: Option<ResMut<AssetServer>>,
    settings: Option<Res<ContentSettings>>,
    mut events: MessageWriter<ChunkStateChanged>,
) {
    let Some(mut manager) = manager else {
        return;
    };
    let budget = settings.map_or(8 << 20, |s| s.step_budget);
    let mut server = server;
    manager.advance(budget, server.as_deref_mut());
    for event in manager.drain_events() {
        events.write(event);
    }
}

/// Reads the content manifest at startup and steps the [`ContentManager`] every frame. Without a
/// manifest in the content directory (or without a content directory) it does nothing, which is
/// today's behaviour.
#[derive(Clone, Debug, Default)]
pub struct ContentPlugin {
    /// Settings; `None` derives them from the asset server's content directory.
    pub settings: Option<ContentSettings>,
}

impl Plugin for ContentPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<ChunkStateChanged>()
            .add_systems(PreUpdate, content_tick);
    }

    fn finish(&self, app: &mut App) {
        let world = app.world_mut();
        let settings = self.settings.clone().or_else(|| {
            world
                .get_resource::<AssetServer>()
                .and_then(|s| s.settings_content_dir().map(ContentSettings::for_content_dir))
        });
        let Some(settings) = settings else {
            return;
        };
        let mut manager = match ContentManager::open(&settings) {
            Ok(m) => m,
            Err(ContentError::ManifestMissing) => return,
            Err(e) => {
                tracing::warn!(target: "regia_asset", "content manifest not used: {e}");
                return;
            }
        };
        let mut host_added = false;
        if let Some(url) = &settings.base_url {
            match HttpSource::new(url, &manager.manifest().build_id) {
                Ok(source) => {
                    manager.add_source(Box::new(source));
                    host_added = true;
                }
                Err(why) => tracing::warn!(target: "regia_asset", "content base URL not used: {why}"),
            }
        }
        // Android: fast-follow and on-demand chunks come from the store's asset packs.
        #[cfg(target_os = "android")]
        manager.add_source(Box::new(super::store::StorePackSource::platform()));
        // The platform says whether the network is metered (spec 40 § 2.5; desktop cannot tell).
        manager.set_metered_probe(Some(platform_metered_probe()));
        // The store starts fast-follow downloads after install, but they may still be running, or the
        // player or the network may have stopped them, and its local testing mode never starts them:
        // the app checks them at every launch (ADR-7696). Off Android only a content host fetches them.
        start_fast_follow(&mut manager, host_added || cfg!(target_os = "android"));
        if let Some(mut server) = world.get_resource_mut::<AssetServer>() {
            manager.sync_with_server(&mut server, settings.mount_installed_at_startup);
        }
        world.insert_resource(manager);
        world.insert_resource(settings);
    }
}

#[cfg(test)]
mod fast_follow_startup_tests {
    //! ADR-7696: the content plugin's launch check of fast-follow chunks against the store.

    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use parking_lot::Mutex;
    use regia_platform::asset_packs::{PackState, status};

    use super::super::manifest::{ChunkEntry, ContentManifest, Delivery, hash_file};
    use super::super::store::{PackStore, StorePackSource};
    use super::{ChunkState, ContentManager, start_fast_follow};

    /// A store whose pack states the test sets; a fetch puts the pack in PENDING, as the activity
    /// helper does.
    #[derive(Default)]
    struct Store {
        states: Mutex<BTreeMap<String, PackState>>,
        locations: Mutex<BTreeMap<String, PathBuf>>,
        calls: Mutex<Vec<String>>,
    }

    impl Store {
        fn set(&self, pack: &str, status: i32, done: u64, total: u64) {
            self.states.lock().insert(
                pack.to_owned(),
                PackState {
                    status,
                    error_code: 0,
                    bytes_downloaded: done,
                    total_bytes: total,
                },
            );
        }
    }

    impl PackStore for Store {
        fn state(&self, pack: &str) -> Result<Option<PackState>, String> {
            Ok(self.states.lock().get(pack).copied())
        }
        fn fetch(&self, pack: &str) -> Result<(), String> {
            self.calls.lock().push(format!("fetch {pack}"));
            self.set(pack, status::PENDING, 0, 0);
            Ok(())
        }
        fn cancel(&self, pack: &str) {
            self.calls.lock().push(format!("cancel {pack}"));
        }
        fn remove(&self, pack: &str) {
            self.calls.lock().push(format!("remove {pack}"));
        }
        fn confirm(&self) {
            self.calls.lock().push("confirm".into());
        }
        fn location(&self, pack: &str) -> Result<Option<PathBuf>, String> {
            Ok(self.locations.lock().get(pack).cloned())
        }
    }

    /// `base` (install) in `content/`; fast-follow `ff-world` and on-demand `extra` in the store's
    /// pack directories.
    fn fixture(tag: &str) -> (PathBuf, ContentManifest) {
        let root = regia_test_utils::unique_temp_dir(tag);
        let content = root.join("content");
        std::fs::create_dir_all(&content).expect("mkdir");
        std::fs::write(content.join("base.fpak"), b"base").expect("base");
        let mut manifest = ContentManifest::new("1.0-1-ff", "android");
        for (name, dir, delivery) in [
            ("base", content.clone(), Delivery::Install),
            ("ff-world", root.join("store/ff_world"), Delivery::FastFollow),
            ("extra", root.join("store/extra"), Delivery::OnDemand),
        ] {
            std::fs::create_dir_all(&dir).expect("mkdir");
            let file = format!("{name}.fpak");
            let bytes: Vec<u8> = (0..4_096u32)
                .map(|i| (i % 249) as u8 ^ name.len() as u8)
                .collect();
            if delivery != Delivery::Install {
                std::fs::write(dir.join(&file), &bytes).expect("pack");
            }
            let path = dir.join(&file);
            manifest.chunks.push(ChunkEntry {
                name: name.into(),
                size: std::fs::metadata(&path).expect("file").len(),
                hash: hash_file(&path).expect("hash"),
                file,
                delivery,
                requires: Vec::new(),
                overrides: false,
                mount_order: 0,
                assets: 1,
                blocks: Vec::new(),
            });
        }
        (root, manifest)
    }

    fn manager(root: &std::path::Path, manifest: ContentManifest, store: &Arc<Store>) -> ContentManager {
        let mut m = ContentManager::from_manifest(manifest, &root.join("content"), &root.join("install"));
        m.add_source(Box::new(StorePackSource::new(
            Arc::clone(store) as Arc<dyn PackStore>
        )));
        m
    }

    /// The store finished the fast-follow pack while the app was not running: the launch check
    /// takes it in place (no fetch) and never touches the on-demand pack.
    #[test]
    fn a_finished_fast_follow_pack_is_taken_at_launch() {
        let (root, manifest) = fixture("ff-launch-done");
        let store = Arc::new(Store::default());
        store.set("ff_world", status::COMPLETED, 9, 9);
        store
            .locations
            .lock()
            .insert("ff_world".into(), root.join("store/ff_world"));
        let mut m = manager(&root, manifest, &store);
        assert_eq!(m.state("ff-world"), Some(ChunkState::NotInstalled));
        let req = start_fast_follow(&mut m, true).expect("a fast-follow request");
        m.advance(0, None);
        assert_eq!(m.state("ff-world"), Some(ChunkState::Installed));
        assert_eq!(m.progress(req).expect("progress").state, "Installed");
        assert_eq!(m.state("extra"), Some(ChunkState::NotInstalled));
        assert!(
            store.calls.lock().is_empty(),
            "nothing fetched: {:?}",
            store.calls.lock()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The store is still downloading the fast-follow pack at launch: the request follows the
    /// store's bytes and installs the pack when the store completes it.
    #[test]
    fn a_running_fast_follow_download_is_followed_to_the_end() {
        let (root, manifest) = fixture("ff-launch-running");
        let store = Arc::new(Store::default());
        store.set("ff_world", status::DOWNLOADING, 300, 1_000);
        let mut m = manager(&root, manifest, &store);
        start_fast_follow(&mut m, true).expect("a fast-follow request");
        m.advance(0, None);
        store.set("ff_world", status::DOWNLOADING, 500, 1_000);
        m.advance(0, None);
        assert_eq!(
            m.state("ff-world"),
            Some(ChunkState::Downloading {
                done: 2_048,
                total: 4_096
            })
        );
        store
            .locations
            .lock()
            .insert("ff_world".into(), root.join("store/ff_world"));
        store.set("ff_world", status::COMPLETED, 1_000, 1_000);
        m.advance(0, None);
        assert_eq!(m.state("ff-world"), Some(ChunkState::Installed));
        assert_eq!(store.calls.lock().clone(), ["fetch ff_world"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The player cancelled the store's fast-follow download (or the network stopped it): the
    /// launch check fetches it again instead of leaving the chunk missing.
    #[test]
    fn a_cancelled_fast_follow_download_is_fetched_again() {
        let (root, manifest) = fixture("ff-launch-cancelled");
        let store = Arc::new(Store::default());
        store.set("ff_world", status::CANCELED, 100, 1_000);
        let mut m = manager(&root, manifest, &store);
        start_fast_follow(&mut m, true).expect("a fast-follow request");
        m.advance(0, None);
        assert_eq!(store.calls.lock().clone(), ["fetch ff_world"]);
        assert!(
            matches!(m.state("ff-world"), Some(ChunkState::Downloading { done: 0, .. })),
            "{:?}",
            m.state("ff-world")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Without a source that can fetch (desktop with no content host) nothing is requested.
    #[test]
    fn no_fetching_source_requests_nothing() {
        let (root, manifest) = fixture("ff-launch-none");
        let store = Arc::new(Store::default());
        let mut m = manager(&root, manifest, &store);
        assert!(start_fast_follow(&mut m, false).is_none());
        m.advance(0, None);
        assert_eq!(m.state("ff-world"), Some(ChunkState::NotInstalled));
        assert!(store.calls.lock().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
