// SPDX-License-Identifier: MPL-2.0
//! Cloud saves (specs/24 § 2 "Cloud saves").
//!
//! M19 delivers **Steam Auto-Cloud** of `user://`: the partner-site configuration below, emitted
//! by `regia package --steam` as `steamcloud.md`, keeps Regia Engine's standard user directory in
//! sync without game code. The [`SaveBackend`] trait is the seam the framework's `SaveGame`
//! plugs into: [`SteamRemoteStorage`] implements it over Steam Remote Storage (the API a game
//! controls file by file), and the framework's `SaveGame::encode`/`decode` byte seam
//! (`regia_framework`, M20) produces the bytes a backend stores.
//!
//! Conflict handling surfaces to the game as [`CloudConflict`] (specs/24 § 2). The frame pump
//! writes one message per [`crate::SteamEvent::CloudConflict`]; games read `Messages<CloudConflict>`
//! and choose which copy to keep. **Only the mock backend produces that event today**
//! ([`MockSteam::sync_remote_file`](crate::MockSteam::sync_remote_file) raises it in tests); the
//! real Steamworks bindings expose no conflict callback, so a live Remote Storage write simply
//! overwrites the cloud copy and games must compare timestamps themselves. Auto-Cloud of `user://` still resolves its own conflicts by Steam's
//! rules before the game starts; this message is the path the game controls (Remote Storage).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use regia_app::prelude::*;

/// A process-unique counter for the local backend's per-write temp file: `SaveBackend` is usable
/// from any thread, so two threads saving one slot must not share a temp name.
static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

/// Leftover temp names one save steps past before it gives up (see [`LocalSaveBackend`]'s
/// `save`): each is a file an earlier process with this pid left when it died mid-save.
const TEMP_NAME_SKIPS: u32 = 64;

/// A Steam Cloud file disagrees with the local slot (specs/24 § 2, "conflict handling
/// surfaced to the game").
///
/// One message per conflict the backend reports. `slot` is the save-slot name (`Chapter1`
/// for a file `saves/Chapter1.frts`); `file` is the cloud name unchanged. The game decides
/// whether to upload its copy or load the remote one — the engine does not pick.
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct CloudConflict {
    /// Save slot the file belongs to.
    pub slot: String,
    /// Cloud file name as the backend reported it.
    pub file: String,
    /// `true` when the remote copy is newer than the one the game last wrote.
    pub remote_newer: bool,
}

/// Slot name from a Remote Storage file (`saves/Chapter1.frts` → `Chapter1`).
///
/// A name that is not in that shape is returned unchanged, so the game can still show it.
pub fn slot_from_cloud_file(file: &str) -> String {
    let base = file.rsplit(['/', '\\']).next().unwrap_or(file);
    base.strip_suffix(".frts").unwrap_or(base).to_owned()
}

/// Where a save slot's bytes live: the backend [`crate`] implements for the framework's
/// `SaveGame`. Implementations must be usable from any thread.
///
/// `regia_framework`'s `SaveGame::encode`/`decode` produce and consume the bytes; a
/// `SaveBackend` decides only where they are stored. [`LocalSaveBackend`] (files under
/// `user://saves/`) and [`SteamRemoteStorage`] (Steam Remote Storage) are the two shipped
/// implementations.
pub trait SaveBackend: Send + Sync {
    /// Writes `data` to `slot` atomically (never leaves a torn save behind).
    ///
    /// # Errors
    /// Any IO or backend failure; the previous slot content must remain intact.
    fn save(&self, slot: &str, data: &[u8]) -> Result<(), SaveError>;

    /// Reads a slot; `Ok(None)` when the slot does not exist.
    ///
    /// # Errors
    /// Any IO or backend failure.
    fn load(&self, slot: &str) -> Result<Option<Vec<u8>>, SaveError>;

    /// Deletes a slot; deleting a missing slot is `Ok(())`.
    ///
    /// # Errors
    /// Any IO or backend failure.
    fn delete(&self, slot: &str) -> Result<(), SaveError>;

    /// Whether the slot exists.
    ///
    /// # Errors
    /// Any IO or backend failure.
    fn exists(&self, slot: &str) -> Result<bool, SaveError>;

    /// Lists every slot name of this backend.
    ///
    /// # Errors
    /// Any IO or backend failure.
    fn slots(&self) -> Result<Vec<String>, SaveError>;
}

/// A save operation failed. Slot names are included for logs and UI.
#[derive(thiserror::Error, Debug)]
pub enum SaveError {
    /// The underlying storage rejected or failed the operation.
    #[error("save operation failed: {0}")]
    Io(#[from] std::io::Error),
    /// The slot name is not usable on this backend (empty, path separators, characters or device names Windows forbids, too long, or
    /// differing only by letter case from an existing slot).
    #[error("invalid save slot name `{0}`")]
    InvalidSlot(String),
    /// Steam Remote Storage is not available (Steam not running / feature off). The framework
    /// should fall back to [`LocalSaveBackend`] (M20).
    #[error("Steam Remote Storage is unavailable")]
    Unavailable,
}

// `io::Error` is not `PartialEq`; tests (and games asserting on errors) care about the variant
// and the io *kind*, not the formatted message.
impl PartialEq for SaveError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Io(a), Self::Io(b)) => a.kind() == b.kind(),
            (Self::InvalidSlot(a), Self::InvalidSlot(b)) => a == b,
            (Self::Unavailable, Self::Unavailable) => true,
            _ => false,
        }
    }
}
impl Eq for SaveError {}

/// Creates `tmp` exclusively, writes `data`, and syncs it.
///
/// `create_new` refuses an existing path, including a symbolic link, so this never follows a
/// link planted at the temp name. A failed write removes only the file this call created. A
/// failed `create_new` (the name already exists) removes nothing and reports `AlreadyExists` on
/// every platform: Windows answers `create_new` on an existing directory with
/// `ERROR_ACCESS_DENIED` (`PermissionDenied`), not `ERROR_FILE_EXISTS`, so a taken name that is
/// a directory is mapped back to `AlreadyExists` and the caller skips to the next name.
fn write_exclusive_temp(tmp: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(tmp) {
        Ok(file) => file,
        Err(err)
            if err.kind() == std::io::ErrorKind::PermissionDenied
                && std::fs::symlink_metadata(tmp).is_ok() =>
        {
            return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, err));
        }
        Err(err) => return Err(err),
    };
    let written = file.write_all(data).and_then(|()| file.sync_all());
    // Drop the handle before removing the file: Windows refuses to delete an open file.
    drop(file);
    if let Err(err) = written {
        let _ = std::fs::remove_file(tmp);
        return Err(err);
    }
    Ok(())
}

/// Longest slot name in bytes: Steam's `k_cchFilenameMax` is 260 including the terminator, and
/// the longest decoration (`saves/` + `.frts`) adds 11 bytes.
const MAX_SLOT_LEN: usize = 248;

/// Checks a slot name is usable as a file name on every platform the save may sync to (a slot
/// saved on Linux or Steam Deck can Auto-Cloud to Windows or macOS): no separators, no
/// characters Windows forbids, no Windows device names, no trailing dot or space, and short
/// enough for a Remote Storage file name.
fn validate_slot(slot: &str) -> Result<(), SaveError> {
    let invalid = || SaveError::InvalidSlot(slot.to_owned());
    if slot.is_empty() || slot == "." || slot == ".." || slot.len() > MAX_SLOT_LEN {
        return Err(invalid());
    }
    if slot.contains(|c: char| c.is_control() || "/\\:<>\"|?*".contains(c)) {
        return Err(invalid());
    }
    if slot.ends_with(['.', ' ']) {
        return Err(invalid());
    }
    // Windows treats `nul`, `nul.txt`, `COM1.sav`... as devices in any directory.
    let stem = slot.split('.').next().unwrap_or(slot).trim_end();
    let lower = stem.to_ascii_lowercase();
    let reserved = matches!(lower.as_str(), "con" | "prn" | "aux" | "nul")
        || ["com", "lpt"].iter().any(|prefix| {
            lower
                .strip_prefix(prefix)
                .is_some_and(|n| matches!(n.as_bytes(), [b'1'..=b'9']))
        });
    if reserved {
        return Err(invalid());
    }
    Ok(())
}

/// Whether `slot` differs only by letter case from an existing slot. Windows and the default
/// macOS volume treat those as one file, so a case-sensitive host (Linux, Deck) must not create
/// the pair.
fn case_conflict(existing: &[String], slot: &str) -> bool {
    existing
        .iter()
        .any(|other| other != slot && other.to_lowercase() == slot.to_lowercase())
}

/// The filesystem [`SaveBackend`] every platform always has: slots under `user://saves/`
/// (`<data dir>/RegiaEngine/<exe>/saves`, see `regia_platform::paths::user_dir`) — exactly the
/// directory the Auto-Cloud configuration below keeps in sync, so Auto-Cloud of `user://` is a
/// save backend with zero Steam code.
#[derive(Debug, Clone)]
pub struct LocalSaveBackend {
    root: PathBuf,
}

impl LocalSaveBackend {
    /// Creates the backend rooted at `<user dir>/saves` — the standard `user://` directory the
    /// Auto-Cloud configuration above syncs. Desktop only: Android's user data lives inside the
    /// app package (its path comes from the app handle; the framework wires it in M20), so there
    /// this returns [`Err`](SaveError::Unavailable) and callers use [`LocalSaveBackend::new`].
    pub fn user_saves() -> Result<Self, SaveError> {
        #[cfg(not(target_os = "android"))]
        {
            let user = regia_platform::paths::user_dir().ok_or(SaveError::Unavailable)?;
            Ok(Self {
                root: user.join("saves"),
            })
        }
        #[cfg(target_os = "android")]
        {
            Err(SaveError::Unavailable)
        }
    }

    /// Creates the backend rooted at an explicit directory (tests point at a temp dir).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory this backend stores slots in (the Auto-Cloud synced path).
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn slot_path(&self, slot: &str) -> Result<PathBuf, SaveError> {
        validate_slot(slot)?;
        Ok(self.root.join(format!("{slot}.sav")))
    }
}

impl SaveBackend for LocalSaveBackend {
    fn save(&self, slot: &str, data: &[u8]) -> Result<(), SaveError> {
        let path = self.slot_path(slot)?;
        if case_conflict(&self.slots()?, slot) {
            return Err(SaveError::InvalidSlot(slot.to_owned()));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Atomic: write beside the target under a per-write unique temp name (the trait is
        // usable from any thread, so a fixed name per slot would let two concurrent saves of
        // one slot interleave), flush it to disk, then rename over it (`rename` replaces an
        // existing file on every platform), so a crash leaves the old or the new slot, never a
        // zero-length one. The temp is created new and removed if the write or the rename
        // fails, so a leftover is not picked up by Auto-Cloud's `*` pattern, and a symlink
        // planted at the temp name is not followed.
        // The temp name carries no slot: a slot at `MAX_SLOT_LEN` plus the decoration would
        // cross the 255-byte file-name limit, and the pid plus the global counter already make
        // the name unique per write in this process.
        // A name that exists already is the leftover of an earlier process with this pid that
        // died mid-save: pids repeat, and the counter restarts with each process. It is not
        // followed and not removed (it is not this save's file); the save takes the next name.
        // Stopping there failed the first save of every later process with that pid.
        let mut skipped = 0u32;
        let tmp = loop {
            let n = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
            let tmp = path.with_file_name(format!(".sav-tmp-{}-{n}.tmp", std::process::id()));
            match write_exclusive_temp(&tmp, data) {
                Ok(()) => break tmp,
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && skipped < TEMP_NAME_SKIPS => {
                    skipped += 1;
                }
                Err(err) => return Err(err.into()),
            }
        };
        if let Err(err) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(err.into());
        }
        #[cfg(unix)]
        {
            if let Some(parent) = path.parent()
                && let Ok(dir) = std::fs::File::open(parent)
            {
                let _ = dir.sync_all(); // best effort: persists the rename itself
            }
        }
        Ok(())
    }

    fn load(&self, slot: &str) -> Result<Option<Vec<u8>>, SaveError> {
        let path = self.slot_path(slot)?;
        match std::fs::read(&path) {
            Ok(data) => Ok(Some(data)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    fn delete(&self, slot: &str) -> Result<(), SaveError> {
        let path = self.slot_path(slot)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    fn exists(&self, slot: &str) -> Result<bool, SaveError> {
        Ok(self.slot_path(slot)?.is_file())
    }

    fn slots(&self) -> Result<Vec<String>, SaveError> {
        let mut names = Vec::new();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(names),
            Err(err) => return Err(err.into()),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            // A directory named `backup.sav` is not a slot.
            if !entry.file_type()?.is_file() {
                continue;
            }
            if let Some(slot) = name.strip_suffix(".sav") {
                names.push(slot.to_owned());
            }
        }
        names.sort();
        Ok(names)
    }
}

/// The Steam Remote Storage [`SaveBackend`] (specs/24 § 2): save slots stored as cloud files
/// named `saves/<slot>.frts`, read and written through the live Steam client.
///
/// Build it from the [`Steam`](crate::Steam) resource (which shares the plugin's backend, mock
/// or real):
///
/// ```ignore
/// let backend = SteamRemoteStorage::new(&steam);
/// let bytes = SaveGame::encode(world, &SaveSpec::default())?;
/// backend.save("Chapter1", &bytes)?;
/// ```
///
/// Without a running Steam client every operation fails with
/// [`SaveError::Unavailable`] — the game falls back to [`LocalSaveBackend`] (or Steam
/// Auto-Cloud keeps `user://` in sync with no game code at all; both routes are valid,
/// Remote Storage is the one the game controls).
#[derive(Clone)]
pub struct SteamRemoteStorage {
    backend: std::sync::Arc<dyn crate::backend::SteamBackend>,
}

impl std::fmt::Debug for SteamRemoteStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SteamRemoteStorage").finish_non_exhaustive()
    }
}

impl SteamRemoteStorage {
    /// A remote-storage backend sharing `steam`'s connection (mock in tests, the real client
    /// in a Steam build).
    pub fn new(steam: &crate::Steam) -> Self {
        Self {
            backend: steam.backend.clone(),
        }
    }

    /// Whether the backend can reach Steam right now (state Ready).
    pub fn is_available(&self) -> bool {
        self.backend.state() == crate::backend::SteamState::Ready
    }

    /// The cloud file name slot `slot` maps to.
    fn file_name(slot: &str) -> Result<String, SaveError> {
        validate_slot(slot)?;
        Ok(format!("saves/{slot}.frts"))
    }
}

impl SaveBackend for SteamRemoteStorage {
    fn save(&self, slot: &str, data: &[u8]) -> Result<(), SaveError> {
        let name = Self::file_name(slot)?;
        if case_conflict(&self.slots()?, slot) {
            return Err(SaveError::InvalidSlot(slot.to_owned()));
        }
        self.backend.cloud_write(&name, data).map_err(steam_save_error)
    }

    fn load(&self, slot: &str) -> Result<Option<Vec<u8>>, SaveError> {
        let name = Self::file_name(slot)?;
        self.backend.cloud_read(&name).map_err(steam_save_error)
    }

    fn delete(&self, slot: &str) -> Result<(), SaveError> {
        let name = Self::file_name(slot)?;
        self.backend.cloud_delete(&name).map_err(steam_save_error)
    }

    fn exists(&self, slot: &str) -> Result<bool, SaveError> {
        let name = Self::file_name(slot)?;
        self.backend.cloud_exists(&name).map_err(steam_save_error)
    }

    fn slots(&self) -> Result<Vec<String>, SaveError> {
        let names = self.backend.cloud_list().map_err(steam_save_error)?;
        Ok(names
            .into_iter()
            .filter_map(|name| {
                let stripped = name.strip_prefix("saves/")?.strip_suffix(".frts")?;
                (!stripped.is_empty()).then(|| stripped.to_owned())
            })
            .collect())
    }
}

/// Maps a [`crate::SteamError`] onto the backend's [`SaveError`]: unavailable stays
/// unavailable (the caller's fallback signal), everything else becomes an IO-flavoured error
/// carrying the message.
fn steam_save_error(error: crate::SteamError) -> SaveError {
    match error {
        crate::SteamError::Unavailable => SaveError::Unavailable,
        other => SaveError::Io(std::io::Error::other(other.to_string())),
    }
}

/// The recovery policy after a failed cloud chunk write, shared by the real backend and its test.
///
/// `steamworks` 0.13's `SteamFileWriter` commits its stream on `drop` (`FileWriteStreamClose`),
/// even after a failed chunk, and exposes no cancel binding — so a failed `write_all` leaves the
/// torn bytes committed under the file name. `previous` is what the file held before the attempt:
/// those bytes are written back, and the file is deleted only when it did not exist before. That
/// is as close to [`SaveBackend::save`]'s contract ("a failed write leaves the previous slot
/// intact") as the bindings allow; a restore that also fails is reported.
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
pub(crate) fn recover_failed_write(
    previous: Option<&[u8]>,
    rewrite: impl FnOnce(&[u8]) -> Result<(), std::io::Error>,
    delete: impl FnOnce(),
) -> Result<(), std::io::Error> {
    match previous {
        Some(bytes) => rewrite(bytes),
        None => {
            delete();
            Ok(())
        }
    }
}

/// Renders the exact Auto-Cloud configuration for the package output (`steamcloud.md`): what to
/// enter on the Steamworks partner site so `user://` (settings and game-written files — all DO27
/// needs, specs/24 § 2) syncs between machines.
/// The Root values are the partner site's documented roots that cover Regia Engine's `user://`
/// directory `<data dir>/RegiaEngine/<exe name>/`:
/// `%APPDATA%\RegiaEngine\<exe>\` on Windows (`WinAppDataRoaming`),
/// `$XDG_DATA_HOME/RegiaEngine/<exe>/` on Linux (`LinuxXdgDataHome`), and
/// `~/Library/Application Support/RegiaEngine/<exe>/` on macOS (`MacAppSupport`, with the port).
pub fn autocloud_document(exe_name: &str) -> String {
    let mut out = String::new();
    out.push_str("# Steam Auto-Cloud configuration\n\n");
    out.push_str(
        "Regia Engine keeps user data under `<user data dir>/RegiaEngine/<exe name>/` \
         (`user://`). Enter the values below once on the Steamworks partner site:\n\
         **Steamworks → your app → Steam Cloud Settings → Steam Auto-Cloud**\n\n",
    );
    out.push_str("| OS | Root | Subdirectory | Pattern | Recursive |\n|---|---|---|---|---|\n");
    let _ = writeln!(
        out,
        "| Windows | WinAppDataRoaming | RegiaEngine\\{exe_name}\\ | * | yes |"
    );
    let _ = writeln!(
        out,
        "| Linux | LinuxXdgDataHome | RegiaEngine/{exe_name}/ | * | yes |"
    );
    let _ = writeln!(
        out,
        "| macOS (with the port) | MacAppSupport | RegiaEngine/{exe_name}/ | * | yes |"
    );
    out.push_str(
        "\nCross-platform saves: keep the Windows row as the primary Root ([All OSes]) and add \
         Root Overrides for Linux (`LinuxXdgDataHome`) and macOS (`MacAppSupport`) — Valve's \
         documented pattern; without overrides, files partition per OS.\n\n\
         Quota: set `Byte quota per user` and `Number of files allowed per user` on the same page. \
         Small files sync best (Valve: 100 MB max per file write). Each save slot keeps one \
         previous copy beside it (`saves/<slot>.prev`), and a damaged settings or input file is \
         kept as `<name>.bad-<n>` (at most five per file); count them in the file quota.\n\n\
         Steam writes a `steam_autocloud.vdf` marker into each synced directory; the game can \
         ignore it.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> LocalSaveBackend {
        let root = std::env::temp_dir().join(format!("regia_steam_test_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        LocalSaveBackend::new(&root)
    }

    #[test]
    fn repeated_saves_replace_the_slot_and_leave_no_temp_file() {
        let saves = temp_root("atomic_replace");
        for round in 0..3u8 {
            saves.save("slot1", &[round; 64]).expect("save");
            assert_eq!(saves.load("slot1"), Ok(Some(vec![round; 64])));
        }
        let names: Vec<String> = std::fs::read_dir(saves.root())
            .expect("read saves dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["slot1.sav"], "no .tmp file is left behind");
        let _ = std::fs::remove_dir_all(saves.root());
    }

    /// A save that fails after its temp file was written must remove it: on a full disk each
    /// retry would otherwise leave another partial temp in the Auto-Cloud-synced directory
    /// (review of fix-steam-platform, finding 2). A directory at the slot's final name makes
    /// the rename fail deterministically.
    #[test]
    fn a_failed_save_removes_its_temp_file() {
        let root = regia_test_utils::unique_temp_dir("steam_failed_save_tmp");
        let saves = LocalSaveBackend::new(&root);
        std::fs::create_dir(root.join("slot1.sav")).expect("mkdir at the slot's name");
        let err = saves.save("slot1", b"data").expect_err("the rename must fail");
        assert!(matches!(err, SaveError::Io(_)), "{err:?}");
        let residue: Vec<String> = std::fs::read_dir(saves.root())
            .expect("read saves dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert_eq!(residue, Vec::<String>::new(), "a failed save leaked {residue:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A slot at `MAX_SLOT_LEN` must save: the temp name carries the pid and counter, not the
    /// slot, so it stays far under the 255-byte file-name limit (review of
    /// fix-steam-platform, finding 3).
    #[test]
    fn a_max_length_slot_saves() {
        let root = regia_test_utils::unique_temp_dir("steam_max_slot");
        let saves = LocalSaveBackend::new(&root);
        let slot = "s".repeat(MAX_SLOT_LEN);
        saves.save(&slot, b"data").expect("a 248-byte slot saves");
        assert_eq!(saves.load(&slot), Ok(Some(b"data".to_vec())));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_saves_of_one_slot_leave_one_complete_save() {
        // `SaveBackend` is usable from any thread: two threads saving one slot at once must not
        // share the temp file and tear the save (audit G, finding 3). The barrier lines both
        // writers up inside the write each round; the slot must always end up holding one
        // complete pattern, never a mix.
        let saves = temp_root("concurrent");
        let pattern_a: Vec<u8> = (0..32 * 1024u32).map(|i| (i % 251) as u8).collect();
        let pattern_b: Vec<u8> = (0..32 * 1024u32).map(|i| (255 - (i % 251)) as u8).collect();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..100 {
                    barrier.wait();
                    saves.save("slot1", &pattern_a).expect("save a");
                }
            });
            scope.spawn(|| {
                for _ in 0..100 {
                    barrier.wait();
                    saves.save("slot1", &pattern_b).expect("save b");
                }
            });
        });
        let final_save = saves.load("slot1").expect("load").expect("a save");
        assert!(
            final_save == pattern_a || final_save == pattern_b,
            "the slot holds a torn save ({} bytes)",
            final_save.len()
        );
        // No temp residue is left behind.
        let residue: Vec<String> = std::fs::read_dir(saves.root())
            .expect("read saves dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert_eq!(residue, Vec::<String>::new(), "{residue:?}");
        let _ = std::fs::remove_dir_all(saves.root());
    }

    #[test]
    fn a_failed_save_leaves_no_temp_file_and_keeps_the_previous_slot() {
        // Rename onto a directory fails after the temp file has been created. The failed save
        // must remove that temp (Auto-Cloud's `*` pattern would otherwise upload it) and must
        // not destroy what was already at the slot path.
        let root = regia_test_utils::unique_temp_dir("steam-save-fail");
        let saves = LocalSaveBackend::new(&root);
        let slot_dir = root.join("slot1.sav");
        std::fs::create_dir_all(&slot_dir).expect("slot dir");
        std::fs::write(slot_dir.join("keep"), b"previous").expect("marker");
        let err = saves
            .save("slot1", b"new-bytes")
            .expect_err("rename onto a directory");
        assert!(matches!(err, SaveError::Io(_)), "{err:?}");
        let temps: Vec<String> = std::fs::read_dir(&root)
            .expect("read root")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert_eq!(temps, Vec::<String>::new(), "failed save left {temps:?}");
        assert_eq!(
            std::fs::read(slot_dir.join("keep")).expect("marker survives"),
            b"previous"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_exclusive_temp_leaves_an_existing_path_alone() {
        // `create_new` fails when the temp name is already taken. That failure must not delete
        // the existing path (a directory here; a symlink is covered below).
        let root = regia_test_utils::unique_temp_dir("steam-save-temp-exists");
        let tmp = root.join("slot.sav.exists.tmp");
        std::fs::create_dir(&tmp).expect("dir");
        std::fs::write(tmp.join("keep"), b"secret").expect("marker");
        let err = write_exclusive_temp(&tmp, b"overwrite").expect_err("existing path");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(tmp.join("keep")).expect("kept"), b"secret");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn write_exclusive_temp_does_not_follow_a_symlink() {
        // `File::create` follows a symlink. The temp must be created new, so a symlink at the
        // temp path fails the write and leaves both the link and its target intact.
        let root = regia_test_utils::unique_temp_dir("steam-save-symlink");
        let secret = root.join("secret");
        std::fs::write(&secret, b"keep-me").expect("secret");
        let tmp = root.join("slot.sav.symlink.tmp");
        std::os::unix::fs::symlink(&secret, &tmp).expect("symlink");
        let err = write_exclusive_temp(&tmp, b"overwrite").expect_err("symlink");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&secret).expect("secret survives"), b"keep-me");
        assert!(
            std::fs::symlink_metadata(&tmp)
                .expect("meta")
                .file_type()
                .is_symlink(),
            "the planted symlink was removed"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The longest slot name the validator accepts must save and load: the temp file used to
    /// be `<slot>.sav.<pid>-<n>.tmp`, past the 255-byte file name limit for slot names from
    /// about 237 bytes, so those saves always failed with an IO error.
    #[test]
    fn the_longest_valid_slot_name_saves_and_loads() {
        let root = regia_test_utils::unique_temp_dir("steam-save-long-slot");
        let backend = LocalSaveBackend::new(&root);
        let slot = "s".repeat(MAX_SLOT_LEN);
        backend
            .save(&slot, b"long")
            .unwrap_or_else(|err| panic!("a {MAX_SLOT_LEN}-byte slot must save: {err}"));
        assert_eq!(backend.load(&slot).ok().flatten().as_deref(), Some(&b"long"[..]));
        assert_eq!(backend.slots().ok(), Some(vec![slot]));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remote_storage_rejects_a_nul_in_the_slot_name() {
        assert!(matches!(
            SteamRemoteStorage::file_name("a\0b"),
            Err(SaveError::InvalidSlot(_))
        ));
        assert!(SteamRemoteStorage::file_name("Chapter1").is_ok());
    }

    #[test]
    fn save_load_delete_round_trip() {
        let saves = temp_root("roundtrip");
        assert!(!saves.exists("slot1").unwrap_or(false));
        assert_eq!(saves.load("slot1"), Ok(None));

        saves.save("slot1", b"v1").expect("save");
        assert!(saves.exists("slot1").unwrap_or(false));
        assert_eq!(saves.load("slot1"), Ok(Some(b"v1".to_vec())));

        saves.save("slot1", b"v2").expect("overwrite");
        assert_eq!(saves.load("slot1"), Ok(Some(b"v2".to_vec())), "save replaces");

        saves.delete("slot1").expect("delete");
        assert_eq!(saves.load("slot1"), Ok(None));
        saves.delete("slot1").expect("delete missing is ok");
        let _ = std::fs::remove_dir_all(saves.root());
    }

    /// A temp name that already exists does not fail the save. The name is the pid and a counter
    /// that restarts with each process, so a process that died mid-save leaves a name the first
    /// save of every later process with that pid asks for again (pids repeat). The save takes the
    /// next name and leaves the leftover alone: it is not followed and not removed. integ-v048
    /// joined v047's exclusive create (which fails on an existing name) with integ-v048b's
    /// slot-free name, whose own plain create had truncated a leftover.
    #[test]
    fn a_leftover_temp_name_does_not_fail_the_save() {
        let saves = LocalSaveBackend::new(regia_test_utils::unique_temp_dir("steam-save-stale-temp"));
        std::fs::create_dir_all(saves.root()).expect("saves dir");
        // Other tests of this binary save at the same time and take counter values too, so the
        // leftovers cover a run of names from the current value.
        let from = NEXT_TMP.load(Ordering::Relaxed);
        let stale: Vec<PathBuf> = (from..from + 8)
            .map(|n| {
                saves
                    .root()
                    .join(format!(".sav-tmp-{}-{n}.tmp", std::process::id()))
            })
            .collect();
        for path in &stale {
            std::fs::write(path, b"partial").expect("leftover temp");
        }
        saves
            .save("slot1", b"data")
            .expect("a save past the leftover names");
        assert_eq!(saves.load("slot1"), Ok(Some(b"data".to_vec())));
        for path in &stale {
            assert_eq!(std::fs::read(path).expect("left alone"), b"partial");
        }
        assert_eq!(saves.slots(), Ok(vec!["slot1".to_owned()]));
        let _ = std::fs::remove_dir_all(saves.root());
    }

    #[test]
    fn atomic_save_leaves_no_temp_file_behind() {
        let saves = temp_root("atomic");
        saves.save("slot1", b"data").expect("save");
        let tmp = saves.root().join("slot1.sav.tmp");
        assert!(!tmp.exists(), "rename moved the temp file away");
        let _ = std::fs::remove_dir_all(saves.root());
    }

    #[test]
    fn slots_are_listed_sorted_and_sanitized() {
        let saves = temp_root("list");
        saves.save("b", b"1").expect("save");
        saves.save("a", b"2").expect("save");
        // Stray files in the saves directory are not slots (only `*.sav` is listed).
        std::fs::write(saves.root().join("notes.txt"), b"not a slot").expect("stray file");
        std::fs::write(saves.root().join("a.sav.tmp"), b"temp residue").expect("temp file");
        assert_eq!(saves.slots(), Ok(vec!["a".to_owned(), "b".to_owned()]));
        assert!(saves.save("../escape", b"x").is_err(), "path traversal rejected");
        assert!(saves.save("", b"x").is_err(), "empty slot rejected");
        let _ = std::fs::remove_dir_all(saves.root());
    }

    #[test]
    fn slot_names_unusable_on_any_platform_are_rejected() {
        let saves = LocalSaveBackend::new(regia_test_utils::unique_temp_dir("steam-slot-names"));
        let long = "x".repeat(MAX_SLOT_LEN + 1);
        for bad in [
            "nul",
            "NUL",
            "con",
            "Aux",
            "prn",
            "com1",
            "LPT9",
            "nul.backup",
            "com1 ",
            "a<b",
            "a>b",
            "What?",
            "a*b",
            "a|b",
            "a\"b",
            "trailing.",
            "trailing ",
            "tab\tname",
            long.as_str(),
        ] {
            assert_eq!(
                saves.save(bad, b"x"),
                Err(SaveError::InvalidSlot(bad.to_owned())),
                "{bad:?} must be rejected"
            );
            assert_eq!(
                SteamRemoteStorage::file_name(bad),
                Err(SaveError::InvalidSlot(bad.to_owned()))
            );
        }
        // Near-misses stay valid.
        let at_limit = "x".repeat(MAX_SLOT_LEN);
        for good in [
            "com0",
            "com10",
            "lpt",
            "console",
            "nul_save",
            "Chapter 1",
            at_limit.as_str(),
        ] {
            assert!(validate_slot(good).is_ok(), "{good:?} must be accepted");
        }
        let _ = std::fs::remove_dir_all(saves.root());
    }

    #[test]
    fn slots_differing_only_by_case_are_rejected() {
        let saves = LocalSaveBackend::new(regia_test_utils::unique_temp_dir("steam-slot-case"));
        saves.save("Slot", b"1").expect("first");
        saves.save("Slot", b"2").expect("same-case overwrite");
        assert_eq!(
            saves.save("slot", b"3"),
            Err(SaveError::InvalidSlot("slot".to_owned()))
        );
        assert_eq!(saves.load("Slot"), Ok(Some(b"2".to_vec())));
        assert_eq!(saves.slots(), Ok(vec!["Slot".to_owned()]));
        let _ = std::fs::remove_dir_all(saves.root());
    }

    #[test]
    fn directories_named_like_slots_are_not_listed() {
        let saves = LocalSaveBackend::new(regia_test_utils::unique_temp_dir("steam-slot-dirs"));
        saves.save("real", b"1").expect("save");
        std::fs::create_dir_all(saves.root().join("backup.sav")).expect("dir");
        assert_eq!(saves.slots(), Ok(vec!["real".to_owned()]));
        let _ = std::fs::remove_dir_all(saves.root());
    }

    #[test]
    fn remote_storage_rejects_case_only_duplicates() {
        let mock = crate::MockSteam::shared();
        let app = steam_app(mock.clone());
        let steam = app.world().resource::<crate::Steam>().clone();
        let cloud = SteamRemoteStorage::new(&steam);
        cloud.save("Slot", b"1").expect("first");
        assert_eq!(
            cloud.save("slot", b"2"),
            Err(SaveError::InvalidSlot("slot".to_owned()))
        );
        assert_eq!(mock.cloud_files(), vec!["saves/Slot.frts".to_owned()]);
    }

    #[test]
    fn autocloud_document_contains_the_user_dir_paths() {
        let doc = autocloud_document("direct_order_27");
        assert!(doc.contains("WinAppDataRoaming"), "windows root: {doc}");
        assert!(
            doc.contains("RegiaEngine\\direct_order_27\\"),
            "windows subdir: {doc}"
        );
        assert!(doc.contains("LinuxXdgDataHome"), "linux root: {doc}");
        assert!(
            doc.contains("RegiaEngine/direct_order_27/"),
            "linux subdir: {doc}"
        );
        assert!(doc.contains("Root Overrides"), "cross-platform guidance: {doc}");
        // ADR-3102: the `*` pattern also syncs the local previous copies; the quota says so.
        assert!(
            doc.contains("saves/<slot>.prev"),
            "previous copies in the quota: {doc}"
        );
    }

    // --- Steam Remote Storage backend ---------------------------------------

    /// An app with the Steam plugin over a shared mock, pumped once so the `Steam` resource
    /// exists.
    fn steam_app(mock: std::sync::Arc<crate::MockSteam>) -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, crate::SteamPlugin::default().with_backend(mock)));
        app.update();
        app
    }

    #[test]
    fn remote_storage_round_trips_slots() {
        let mock = crate::MockSteam::shared();
        let app = steam_app(mock.clone());
        let steam = app.world().resource::<crate::Steam>().clone();
        let cloud = SteamRemoteStorage::new(&steam);

        assert!(cloud.is_available());
        assert_eq!(cloud.load("slot1"), Ok(None));
        assert!(!cloud.exists("slot1").unwrap_or(false));

        cloud.save("slot1", b"v1").expect("cloud save");
        assert!(cloud.exists("slot1").unwrap_or(false));
        assert_eq!(cloud.load("slot1"), Ok(Some(b"v1".to_vec())));
        // The bytes really went through the "Steam" backend.
        assert_eq!(mock.cloud_file("saves/slot1.frts"), Some(b"v1".to_vec()));

        cloud.save("slot1", b"v2").expect("cloud overwrite");
        assert_eq!(cloud.load("slot1"), Ok(Some(b"v2".to_vec())));

        cloud.save("slot2", b"x").expect("second slot");
        assert_eq!(cloud.slots(), Ok(vec!["slot1".to_owned(), "slot2".to_owned()]));

        cloud.delete("slot1").expect("cloud delete");
        assert_eq!(cloud.load("slot1"), Ok(None));
        cloud.delete("slot1").expect("delete missing is ok");
    }

    #[test]
    fn remote_storage_rejects_bad_slot_names() {
        let mock = crate::MockSteam::shared();
        let app = steam_app(mock);
        let steam = app.world().resource::<crate::Steam>().clone();
        let cloud = SteamRemoteStorage::new(&steam);
        assert!(cloud.save("../escape", b"x").is_err());
        assert!(cloud.save("", b"x").is_err());
    }

    #[test]
    fn remote_storage_reports_unavailable_without_steam() {
        let mock = crate::MockSteam::shared_unavailable();
        let app = steam_app(mock);
        let steam = app.world().resource::<crate::Steam>().clone();
        let cloud = SteamRemoteStorage::new(&steam);
        assert!(!cloud.is_available());
        assert_eq!(cloud.load("slot1"), Err(SaveError::Unavailable));
        assert_eq!(cloud.save("slot1", b"x"), Err(SaveError::Unavailable));
    }

    // --- The failed-write recovery policy (finding: a failed cloud write must not destroy the
    // previous save; `steamworks` 0.13 commits the stream on drop, so the old bytes are written
    // back and the file is deleted only when it did not exist) -----------------

    /// Stands in for `steamworks::SteamFileWriter`: bytes accumulate in memory and "commit" to
    /// the shared buffer on drop even after a failed write, like `FileWriteStreamClose`.
    struct CommittingWriter {
        target: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
        pending: Vec<u8>,
        /// When true, the next `write` fails once (a rejected chunk).
        fail_next: bool,
    }

    impl std::io::Write for CommittingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.fail_next {
                self.fail_next = false;
                return Err(std::io::Error::other("chunk rejected"));
            }
            self.pending.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Drop for CommittingWriter {
        fn drop(&mut self) {
            *self.target.borrow_mut() = std::mem::take(&mut self.pending);
        }
    }

    #[test]
    fn failed_write_restores_the_previous_payload_and_never_deletes() {
        let file = std::rc::Rc::new(std::cell::RefCell::new(b"old save".to_vec()));
        let mut delete_called = false;

        // The attempt: first chunk fails, drop commits the torn (empty) file over the old save.
        let mut writer = CommittingWriter {
            target: std::rc::Rc::clone(&file),
            pending: Vec::new(),
            fail_next: true,
        };
        let attempted = std::io::Write::write_all(&mut writer, b"new save");
        assert!(attempted.is_err());
        drop(writer);
        assert!(
            file.borrow().is_empty(),
            "the torn write committed, as steamworks does"
        );

        // The recovery: the previous bytes are written back; a slot that existed is never deleted.
        recover_failed_write(
            Some(b"old save"),
            |bytes| {
                let mut restore = CommittingWriter {
                    target: std::rc::Rc::clone(&file),
                    pending: Vec::new(),
                    fail_next: false,
                };
                std::io::Write::write_all(&mut restore, bytes)
            },
            || delete_called = true,
        )
        .expect("restore writes");
        assert_eq!(
            &*file.borrow(),
            b"old save",
            "the previous save survived the failed write"
        );
        assert!(
            !delete_called,
            "an existing slot is not deleted after a failed write"
        );
    }

    #[test]
    fn failed_write_of_a_new_slot_deletes_the_torn_file() {
        let mut rewritten = false;
        let mut deleted = false;
        recover_failed_write(
            None,
            |_| {
                rewritten = true;
                Ok(())
            },
            || deleted = true,
        )
        .expect("deleting a torn new slot is not an error");
        assert!(deleted, "a slot that did not exist before is removed");
        assert!(!rewritten, "there is nothing to write back");
    }

    #[test]
    fn failed_restore_is_reported() {
        let mut delete_called = false;
        let result = recover_failed_write(
            Some(b"old save"),
            |_| Err(std::io::Error::other("quota full")),
            || delete_called = true,
        );
        assert!(result.is_err(), "a restore failure surfaces");
        assert!(!delete_called, "deleting an existing slot is never the recovery");
    }
}
