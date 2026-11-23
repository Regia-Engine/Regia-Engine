// SPDX-License-Identifier: MPL-2.0
//! The Android store chunk source (`docs/specs/40-content-chunks-dlc-asset-packs.md` § 2.6,
//! ADR-1876): `fast_follow` and `on_demand` chunks ship as store asset packs, one pack per chunk
//! named [`super::pack_name`]. [`StorePackSource`] asks the store for a pack, reports the store's
//! byte counts as transfer progress, and when the pack is on the device links (or copies) its pak
//! into the manager's `.part` file, so the usual BLAKE3 verification and install follow. A pack
//! already on the device is hashed where it lies, once per file stamp.
//!
//! The store is behind the [`PackStore`] trait: [`PlatformPackStore`] calls
//! `regia_platform::asset_packs` (JNI on Android, unsupported elsewhere), and tests drive a scripted
//! store on any host. [`chunk_state_for`] is the store-status to [`ChunkState`] table.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use regia_platform::asset_packs::{self, PackState, status};

use super::manifest::{ChunkEntry, Delivery, pack_name};
use super::source::{ChunkSource, SourceStatus, Transfer, TransferStep};
use super::{ChunkState, FailReason};

/// Start of a transfer failure message for a store status this build does not map; the manager
/// turns it into `Failed(store_unknown_status)`.
pub const STORE_UNKNOWN_STATUS: &str = "store_unknown_status";

/// The store's asset-pack calls (see `regia_platform::asset_packs`).
pub trait PackStore: Send + Sync {
    /// The pack's last known state; `Ok(None)` until the store has answered.
    ///
    /// # Errors
    /// The store is not available (not Android, a build without packs, a JNI failure).
    fn state(&self, pack: &str) -> Result<Option<PackState>, String>;
    /// Requests the pack.
    ///
    /// # Errors
    /// As [`PackStore::state`].
    fn fetch(&self, pack: &str) -> Result<(), String>;
    /// Cancels a running download (best effort).
    fn cancel(&self, pack: &str);
    /// Removes the pack from the device (best effort).
    fn remove(&self, pack: &str);
    /// Shows the store's confirmation dialog (best effort).
    fn confirm(&self);
    /// The directory of the pack's files once it is on the device.
    ///
    /// # Errors
    /// As [`PackStore::state`].
    fn location(&self, pack: &str) -> Result<Option<PathBuf>, String>;
}

/// The real store through `regia_platform::asset_packs`.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlatformPackStore;

impl PackStore for PlatformPackStore {
    fn state(&self, pack: &str) -> Result<Option<PackState>, String> {
        asset_packs::pack_state(pack).map_err(|e| e.to_string())
    }

    fn fetch(&self, pack: &str) -> Result<(), String> {
        asset_packs::fetch_pack(pack).map_err(|e| e.to_string())
    }

    fn cancel(&self, pack: &str) {
        if let Err(e) = asset_packs::cancel_pack(pack) {
            tracing::debug!(target: "regia_asset", "store: cancel {pack}: {e}");
        }
    }

    fn remove(&self, pack: &str) {
        if let Err(e) = asset_packs::remove_pack(pack) {
            tracing::warn!(target: "regia_asset", "store: remove {pack}: {e}");
        }
    }

    fn confirm(&self) {
        if let Err(e) = asset_packs::show_confirmation() {
            tracing::warn!(target: "regia_asset", "store: confirmation dialog: {e}");
        }
    }

    fn location(&self, pack: &str) -> Result<Option<PathBuf>, String> {
        asset_packs::pack_location(pack).map_err(|e| e.to_string())
    }
}

/// The chunk state a store pack state stands for (spec 40 § 2.6): unknown and not-installed are
/// `NotInstalled`, pending is `Queued`, downloading and transferring are `Downloading`, completed
/// is `Installed`, failed is `Failed(source_unreachable)`, cancelled is `Cancelled`, waiting for
/// Wi-Fi and needing the player's confirmation are `WaitingForUnmetered`; any other status is
/// `Failed(store_unknown_status)`.
#[must_use]
pub fn chunk_state_for(state: &PackState) -> ChunkState {
    match state.status {
        status::UNKNOWN | status::NOT_INSTALLED => ChunkState::NotInstalled,
        status::PENDING => ChunkState::Queued,
        status::DOWNLOADING | status::TRANSFERRING => ChunkState::Downloading {
            done: state.bytes_downloaded.min(state.total_bytes),
            total: state.total_bytes,
        },
        status::COMPLETED => ChunkState::Installed,
        status::FAILED => ChunkState::Failed(FailReason::SourceUnreachable(format!(
            "store error {}",
            state.error_code
        ))),
        status::CANCELED => ChunkState::Cancelled,
        status::WAITING_FOR_WIFI | status::REQUIRES_USER_CONFIRMATION => ChunkState::WaitingForUnmetered,
        _ => ChunkState::Failed(FailReason::StoreUnknownStatus),
    }
}

/// Fetches `fast_follow` and `on_demand` chunks through the store's asset packs.
#[derive(Clone)]
pub struct StorePackSource {
    store: Arc<dyn PackStore>,
}

impl std::fmt::Debug for StorePackSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorePackSource").finish_non_exhaustive()
    }
}

impl StorePackSource {
    /// A source over the platform store (Android; elsewhere every chunk reports missing).
    #[must_use]
    pub fn platform() -> Self {
        Self::new(Arc::new(PlatformPackStore))
    }

    /// A source over `store` (tests, tools).
    #[must_use]
    pub fn new(store: Arc<dyn PackStore>) -> Self {
        Self { store }
    }

    fn is_pack(chunk: &ChunkEntry) -> bool {
        matches!(chunk.delivery, Delivery::FastFollow | Delivery::OnDemand)
    }
}

impl ChunkSource for StorePackSource {
    fn name(&self) -> &'static str {
        "android_pack"
    }

    fn status(&self, chunk: &ChunkEntry) -> SourceStatus {
        if !Self::is_pack(chunk) {
            return SourceStatus::Missing;
        }
        let pack = pack_name(&chunk.name);
        match self.store.state(&pack) {
            Err(_) => SourceStatus::Missing,
            Ok(Some(state)) if state.status == status::COMPLETED => match self.store.location(&pack) {
                Ok(Some(dir)) if dir.join(&chunk.file).is_file() => {
                    SourceStatus::Present(dir.join(&chunk.file))
                }
                _ => SourceStatus::Available,
            },
            Ok(_) => SourceStatus::Available,
        }
    }

    fn start(&self, chunk: &ChunkEntry, dest: &Path) -> Box<dyn Transfer> {
        let pack = pack_name(&chunk.name);
        let started = self.store.fetch(&pack);
        Box::new(StoreTransfer {
            store: Arc::clone(&self.store),
            pack,
            file: chunk.file.clone(),
            size: chunk.size,
            dest: dest.to_path_buf(),
            start_error: started.err(),
            confirmed: false,
            finished: false,
        })
    }

    fn uses_install_space(&self) -> bool {
        false
    }

    fn verify_present(&self) -> bool {
        true
    }

    fn release(&self, chunk: &ChunkEntry) {
        if Self::is_pack(chunk) {
            self.store.remove(&pack_name(&chunk.name));
        }
    }
}

/// Logged once per process: the first unmapped store status.
static UNKNOWN_STATUS_LOGGED: AtomicBool = AtomicBool::new(false);

struct StoreTransfer {
    store: Arc<dyn PackStore>,
    pack: String,
    file: String,
    size: u64,
    dest: PathBuf,
    start_error: Option<String>,
    confirmed: bool,
    finished: bool,
}

impl Drop for StoreTransfer {
    fn drop(&mut self) {
        // Pause, cancel or release before completion: stop the store download too.
        if !self.finished {
            self.store.cancel(&self.pack);
        }
    }
}

impl StoreTransfer {
    /// The store's byte count scaled to the chunk file's size (the store counts download bytes).
    fn scaled(&self, state: &PackState) -> u64 {
        if state.total_bytes == 0 {
            return 0;
        }
        let done = u128::from(state.bytes_downloaded.min(state.total_bytes));
        u64::try_from(done * u128::from(self.size) / u128::from(state.total_bytes)).unwrap_or(self.size)
    }

    /// Hard-links the pack's pak into the `.part` path (same volume: no copy), else copies it.
    fn take_file(&self) -> Result<(), String> {
        let dir = self.store.location(&self.pack)?.ok_or_else(|| {
            format!(
                "the store reports pack {} complete but gives no location",
                self.pack
            )
        })?;
        let src = dir.join(&self.file);
        if let Some(parent) = self.dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        match std::fs::remove_file(&self.dest) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.to_string()),
            _ => {}
        }
        if std::fs::hard_link(&src, &self.dest).is_err() {
            std::fs::copy(&src, &self.dest).map_err(|e| format!("{}: {e}", src.display()))?;
        }
        Ok(())
    }
}

impl Transfer for StoreTransfer {
    fn step(&mut self, _budget: u64) -> TransferStep {
        if let Some(why) = &self.start_error {
            return TransferStep::Failed(format!("store: {why}"));
        }
        let state = match self.store.state(&self.pack) {
            Err(why) => return TransferStep::Failed(format!("store: {why}")),
            Ok(None) => {
                return TransferStep::Progress {
                    done: 0,
                    total: self.size,
                };
            }
            Ok(Some(state)) => state,
        };
        match state.status {
            status::COMPLETED => match self.take_file() {
                Ok(()) => {
                    self.finished = true;
                    TransferStep::Done
                }
                Err(why) => TransferStep::Failed(why),
            },
            status::FAILED => TransferStep::Failed(format!("store error {}", state.error_code)),
            status::CANCELED => TransferStep::Failed(format!("the store cancelled pack {}", self.pack)),
            status::WAITING_FOR_WIFI | status::REQUIRES_USER_CONFIRMATION => {
                if !self.confirmed {
                    self.confirmed = true;
                    self.store.confirm();
                }
                TransferStep::Progress {
                    done: self.scaled(&state),
                    total: self.size,
                }
            }
            status::UNKNOWN
            | status::PENDING
            | status::NOT_INSTALLED
            | status::DOWNLOADING
            | status::TRANSFERRING => TransferStep::Progress {
                done: self.scaled(&state),
                total: self.size,
            },
            other => {
                if !UNKNOWN_STATUS_LOGGED.swap(true, Ordering::Relaxed) {
                    tracing::warn!(target: "regia_asset", "store: unmapped asset pack status {other} for {}", self.pack);
                }
                TransferStep::Failed(format!("{STORE_UNKNOWN_STATUS}: {other}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(status: i32, done: u64, total: u64) -> PackState {
        PackState {
            status,
            error_code: 0,
            bytes_downloaded: done,
            total_bytes: total,
        }
    }

    #[test]
    fn every_store_status_maps() {
        assert_eq!(
            chunk_state_for(&st(status::UNKNOWN, 0, 0)),
            ChunkState::NotInstalled
        );
        assert_eq!(
            chunk_state_for(&st(status::NOT_INSTALLED, 0, 0)),
            ChunkState::NotInstalled
        );
        assert_eq!(chunk_state_for(&st(status::PENDING, 0, 9)), ChunkState::Queued);
        assert_eq!(
            chunk_state_for(&st(status::DOWNLOADING, 5, 9)),
            ChunkState::Downloading { done: 5, total: 9 }
        );
        assert_eq!(
            chunk_state_for(&st(status::TRANSFERRING, 12, 9)),
            ChunkState::Downloading { done: 9, total: 9 }
        );
        assert_eq!(
            chunk_state_for(&st(status::COMPLETED, 9, 9)),
            ChunkState::Installed
        );
        assert!(matches!(
            chunk_state_for(&st(status::FAILED, 0, 9)),
            ChunkState::Failed(FailReason::SourceUnreachable(_))
        ));
        assert_eq!(
            chunk_state_for(&st(status::CANCELED, 0, 9)),
            ChunkState::Cancelled
        );
        assert_eq!(
            chunk_state_for(&st(status::WAITING_FOR_WIFI, 0, 9)),
            ChunkState::WaitingForUnmetered
        );
        assert_eq!(
            chunk_state_for(&st(status::REQUIRES_USER_CONFIRMATION, 0, 9)),
            ChunkState::WaitingForUnmetered
        );
        assert_eq!(
            chunk_state_for(&st(42, 0, 9)),
            ChunkState::Failed(FailReason::StoreUnknownStatus)
        );
    }
}
