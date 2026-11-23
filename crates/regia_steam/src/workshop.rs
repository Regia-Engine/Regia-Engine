// SPDX-License-Identifier: MPL-2.0
//! Steam Workshop: upload and subscribe, and subscribed items mounted at `mods://` (spec 24 § 2,
//! ADR-2418).
//!
//! - [`WorkshopService`] is the seam: upload an item (create or update, then submit a folder),
//!   subscribe, unsubscribe, the subscribed list, an item's state and install folder, and `poll`
//!   for what finished. [`MockWorkshop`] is an in-memory Workshop for tests and dev harnesses (it
//!   "installs" items into a folder of its own, like the Steam client does); `RealWorkshop` talks to
//!   ISteamUGC through the `steamworks` 0.13 bindings and is compiled only with the `steamworks`
//!   feature on desktop. Without either, [`UnavailableWorkshop`] answers `workshop_unavailable`.
//! - [`SteamWorkshop`] is the game-facing resource and [`SteamWorkshopPlugin`] the plugin: a
//!   `PreUpdate` step polls the service and, when an item is installed (or updated), mounts its
//!   folder into [`ModsVfs`] as mod `workshop.<item id>` at the end of the load order; an
//!   unsubscribed item is unmounted. A folder holding one `.fpak` mounts as that pak.
//! - The plugin also keeps the lobby key `mods.content_set` (`MODS_LOBBY_KEY`) of a lobby this player owns equal to
//!   the mounted set's content-set hash, so lobbies match on the exact mods and their order.
//!
//! Uploads go through `mods://` rules first: the content folder is mounted (and so checked for
//! links and bad paths) before anything is sent, and its content hash rides in the item metadata as
//! `regia.mods.content=<hex>` when the upload sets no metadata of its own.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use regia_app::prelude::*;
use regia_asset::mods_vfs::{ModSource, ModsError, ModsVfs};
use regia_ecs::ecs::system::SystemState;

use crate::backend::SteamState;
use crate::resource::Steam;

/// A Workshop item id (Steam's `PublishedFileId_t`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkshopItemId(pub u64);

impl std::fmt::Display for WorkshopItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// An upload request, answered by [`WorkshopEvent::Uploaded`] or [`WorkshopEvent::UploadFailed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkshopRequest(pub u64);

/// The `mods://` id a subscribed item mounts under: `workshop.<item id>`.
#[must_use]
pub fn workshop_mod_id(item: WorkshopItemId) -> String {
    format!("workshop.{}", item.0)
}

/// Who can see an uploaded item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkshopVisibility {
    /// Everyone.
    Public,
    /// The uploader's friends.
    FriendsOnly,
    /// The uploader only (the default: publishing is a separate, deliberate step).
    #[default]
    Private,
    /// Anyone with the link.
    Unlisted,
}

impl WorkshopVisibility {
    /// `public`, `friends_only`, `private` or `unlisted`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::FriendsOnly => "friends_only",
            Self::Private => "private",
            Self::Unlisted => "unlisted",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Public, Self::FriendsOnly, Self::Private, Self::Unlisted]
            .into_iter()
            .find(|v| v.name() == name)
    }
}

/// Longest title in bytes (Steam's `k_cchPublishedDocumentTitleMax` less the terminator).
pub const MAX_TITLE_BYTES: usize = 128;
/// Longest description in bytes (`k_cchPublishedDocumentDescriptionMax`).
pub const MAX_DESCRIPTION_BYTES: usize = 8000;
/// Longest metadata in bytes (`k_cchDeveloperMetadataMax`).
pub const MAX_METADATA_BYTES: usize = 5000;
/// Most tags per item.
pub const MAX_TAGS: usize = 32;
/// Longest tag in bytes (`k_cchTagListMax` per tag is 255; Steam's tag limit is shorter).
pub const MAX_TAG_BYTES: usize = 255;

/// One item to upload: a new item (`item: None`) or an update of one the user owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkshopUpload {
    /// The item to update; `None` creates one.
    pub item: Option<WorkshopItemId>,
    /// Title, 1..=128 bytes.
    pub title: String,
    /// Description, at most 8,000 bytes.
    pub description: String,
    /// The folder whose files become the item's content (checked like a `mods://` mount).
    /// `check()` canonicalizes it to an absolute path.
    pub content_dir: PathBuf,
    /// A preview image file, if any. `check()` canonicalizes it to an absolute path.
    pub preview: Option<PathBuf>,
    /// Tags, at most 32 of at most 255 bytes.
    pub tags: Vec<String>,
    /// Who can see it.
    pub visibility: WorkshopVisibility,
    /// The change note shown in the item's history.
    pub change_note: String,
    /// Developer metadata, at most 5,000 bytes; empty means `regia.mods.content=<hash>`.
    pub metadata: String,
}

impl WorkshopUpload {
    /// A new private item titled `title` holding the files of `content_dir`.
    pub fn new(title: impl Into<String>, content_dir: impl Into<PathBuf>) -> Self {
        Self {
            item: None,
            title: title.into(),
            description: String::new(),
            content_dir: content_dir.into(),
            preview: None,
            tags: Vec::new(),
            visibility: WorkshopVisibility::Private,
            change_note: String::new(),
            metadata: String::new(),
        }
    }

    /// Checks the text limits and the content folder (mounted like `mods://`: no links, no bad
    /// paths), then canonicalizes the content and preview paths so every service receives an
    /// absolute path: the real ISteamUGC takes `SetItemContent`/`SetItemPreview` verbatim, so a
    /// relative path would fail inside Steam as an opaque platform error. Returns the folder's
    /// files as a mounted mod, for its content hash and bytes.
    pub fn check(&mut self) -> Result<ModsVfs, WorkshopError> {
        let invalid = |why: String| Err(WorkshopError::Invalid(why));
        if self.title.is_empty() || self.title.len() > MAX_TITLE_BYTES {
            return invalid(format!("the title must be 1..={MAX_TITLE_BYTES} bytes"));
        }
        if self.description.len() > MAX_DESCRIPTION_BYTES {
            return invalid(format!("the description is over {MAX_DESCRIPTION_BYTES} bytes"));
        }
        if self.metadata.len() > MAX_METADATA_BYTES {
            return invalid(format!("the metadata is over {MAX_METADATA_BYTES} bytes"));
        }
        if self.tags.len() > MAX_TAGS || self.tags.iter().any(|t| t.is_empty() || t.len() > MAX_TAG_BYTES) {
            return invalid(format!("at most {MAX_TAGS} tags of 1..={MAX_TAG_BYTES} bytes"));
        }
        let text = [&self.title, &self.description, &self.metadata, &self.change_note];
        if text.iter().any(|t| t.contains('\0'))
            || self.tags.iter().any(|t| t.contains('\0') || t.contains(','))
        {
            return invalid("text may not hold NUL, and tags may not hold `,`".into());
        }
        if let Some(preview) = &self.preview
            && !preview.is_file()
        {
            return invalid(format!("the preview {} is not a file", preview.display()));
        }
        if !self.content_dir.is_dir() {
            return invalid(format!(
                "the content folder {} is not a folder",
                self.content_dir.display()
            ));
        }
        // The real Workshop takes the paths verbatim: hand every service absolute, canonical
        // paths, so a relative folder the game addressed from its working directory fails here
        // (at the cause), not inside Steam.
        self.content_dir = std::fs::canonicalize(&self.content_dir).map_err(|e| {
            WorkshopError::Invalid(format!(
                "the content folder {} cannot be resolved: {e}",
                self.content_dir.display()
            ))
        })?;
        if let Some(preview) = self.preview.as_mut() {
            *preview = std::fs::canonicalize(&*preview).map_err(|e| {
                WorkshopError::Invalid(format!(
                    "the preview {} cannot be resolved: {e}",
                    preview.display()
                ))
            })?;
        }
        let mut vfs = ModsVfs::new();
        vfs.mount("upload", ModSource::Dir(self.content_dir.clone()))
            .map_err(WorkshopError::Content)?;
        if vfs.file_count() == 0 {
            return invalid(format!(
                "the content folder {} is empty",
                self.content_dir.display()
            ));
        }
        Ok(vfs)
    }

    /// The metadata sent: [`Self::metadata`], or `regia.mods.content=<hash>` of the checked folder.
    #[must_use]
    pub fn metadata_for(&self, checked: &ModsVfs) -> String {
        if !self.metadata.is_empty() {
            return self.metadata.clone();
        }
        checked
            .mods()
            .first()
            .map(|m| format!("regia.mods.content={}", m.content_hash.to_hex()))
            .unwrap_or_default()
    }
}

/// Why a Workshop call failed. [`code`](Self::code) is part of the agent protocol.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkshopError {
    /// No Workshop: Steam is not running, the feature is off, or no service is attached.
    #[error("the Steam Workshop is unavailable")]
    Unavailable,
    /// A parameter broke a limit.
    #[error("invalid Workshop request: {0}")]
    Invalid(String),
    /// The content folder breaks a `mods://` rule (a link, a bad path, too large).
    #[error("the content folder cannot be uploaded: {0}")]
    Content(ModsError),
    /// The item is not known (not on the Workshop, or not subscribed for an unsubscribe).
    #[error("unknown Workshop item {0}")]
    UnknownItem(u64),
    /// Steam refused or failed the call.
    #[error("Steam: {0}")]
    Platform(String),
    /// The host does not let the agent upload to the real Workshop.
    #[error("agent uploads to the Steam Workshop are off in this game")]
    UploadNotAllowed,
}

impl WorkshopError {
    /// Every code, in protocol order.
    pub const CODES: &'static [&'static str] = &[
        "workshop_unavailable",
        "workshop_invalid",
        "workshop_content",
        "workshop_unknown_item",
        "workshop_platform",
        "workshop_upload_not_allowed",
    ];

    /// The protocol string.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable => "workshop_unavailable",
            Self::Invalid(_) => "workshop_invalid",
            Self::Content(_) => "workshop_content",
            Self::UnknownItem(_) => "workshop_unknown_item",
            Self::Platform(_) => "workshop_platform",
            Self::UploadNotAllowed => "workshop_upload_not_allowed",
        }
    }
}

/// Call-time result of unsubscribe (`book/agents/workshop.md`).
///
/// An item the service does not list as subscribed is [`WorkshopError::UnknownItem`]
/// (`workshop_unknown_item`) on the call. A subscribed item returns `Ok` and the platform
/// call runs. A later Steam callback failure is [`WorkshopEvent::SubscribeFailed`] with
/// [`WorkshopError::Platform`] (`workshop_platform`); `steam.workshop_status` shows it as
/// `items[].error` text beginning `unsubscribe failed: Steam:`.
pub(crate) fn unsubscribe_if_subscribed(item: WorkshopItemId, subscribed: bool) -> Result<(), WorkshopError> {
    if subscribed {
        Ok(())
    } else {
        Err(WorkshopError::UnknownItem(item.0))
    }
}

/// What a Workshop service finished since the last poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkshopEvent {
    /// An upload was accepted.
    Uploaded {
        /// The request.
        request: WorkshopRequest,
        /// The item (new or updated).
        item: WorkshopItemId,
        /// The user must accept the Workshop legal agreement before the item becomes visible.
        needs_legal_agreement: bool,
    },
    /// An upload failed.
    UploadFailed {
        /// The request.
        request: WorkshopRequest,
        /// Why.
        error: WorkshopError,
    },
    /// A subscription took effect; the download follows.
    Subscribed {
        /// The item.
        item: WorkshopItemId,
    },
    /// A subscription failed.
    SubscribeFailed {
        /// The item.
        item: WorkshopItemId,
        /// Why.
        error: WorkshopError,
    },
    /// An unsubscribe failed (the item stays subscribed).
    UnsubscribeFailed {
        /// The item.
        item: WorkshopItemId,
        /// Why.
        error: WorkshopError,
    },
    /// A subscribed item's download could not be started; the poll keeps retrying and reports
    /// the first failure of a streak, then a reminder every 120 failed attempts (an attempt is
    /// made on each check of the item, every 30 to 120 polls, so one to four minutes at 60 fps).
    DownloadFailed {
        /// The item.
        item: WorkshopItemId,
        /// Why.
        error: WorkshopError,
    },
    /// An unsubscribe took effect.
    Unsubscribed {
        /// The item.
        item: WorkshopItemId,
    },
    /// A subscribed item is installed (or was updated) in `folder`.
    Installed {
        /// The item.
        item: WorkshopItemId,
        /// Its install folder.
        folder: PathBuf,
    },
}

/// A subscribed item's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorkshopItemState {
    /// The user is subscribed.
    pub subscribed: bool,
    /// Its files are on disk.
    pub installed: bool,
    /// A download is running or queued.
    pub downloading: bool,
    /// A newer version is on the Workshop.
    pub needs_update: bool,
}

/// Where an installed item's files are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledItem {
    /// The install folder.
    pub folder: PathBuf,
    /// Bytes on disk.
    pub size_on_disk: u64,
    /// When it was last updated (Steam's Unix time stamp).
    pub timestamp: u32,
}

/// A Workshop: the real ISteamUGC, the mock, or none. Calls never block: results come back from
/// [`WorkshopService::poll`].
pub trait WorkshopService: Send + Sync {
    /// `steam`, `mock` or `unavailable`.
    fn name(&self) -> &'static str;
    /// Starts an upload (the caller has checked it with [`WorkshopUpload::check`]).
    fn upload(&self, upload: &WorkshopUpload, metadata: &str) -> Result<WorkshopRequest, WorkshopError>;
    /// Subscribes to `item` (and starts its download).
    fn subscribe(&self, item: WorkshopItemId) -> Result<(), WorkshopError>;
    /// Unsubscribes from `item`.
    fn unsubscribe(&self, item: WorkshopItemId) -> Result<(), WorkshopError>;
    /// The subscribed items, sorted.
    fn subscribed(&self) -> Vec<WorkshopItemId>;
    /// An item's state.
    fn item_state(&self, item: WorkshopItemId) -> WorkshopItemState;
    /// An installed item's folder.
    fn install_info(&self, item: WorkshopItemId) -> Option<InstalledItem>;
    /// What finished since the last call.
    fn poll(&self) -> Vec<WorkshopEvent>;
}

/// The service when there is no Workshop: every call is `workshop_unavailable`.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableWorkshop;

impl WorkshopService for UnavailableWorkshop {
    fn name(&self) -> &'static str {
        "unavailable"
    }
    fn upload(&self, _upload: &WorkshopUpload, _metadata: &str) -> Result<WorkshopRequest, WorkshopError> {
        Err(WorkshopError::Unavailable)
    }
    fn subscribe(&self, _item: WorkshopItemId) -> Result<(), WorkshopError> {
        Err(WorkshopError::Unavailable)
    }
    fn unsubscribe(&self, _item: WorkshopItemId) -> Result<(), WorkshopError> {
        Err(WorkshopError::Unavailable)
    }
    fn subscribed(&self) -> Vec<WorkshopItemId> {
        Vec::new()
    }
    fn item_state(&self, _item: WorkshopItemId) -> WorkshopItemState {
        WorkshopItemState::default()
    }
    fn install_info(&self, _item: WorkshopItemId) -> Option<InstalledItem> {
        None
    }
    fn poll(&self) -> Vec<WorkshopEvent> {
        Vec::new()
    }
}

/// Records one "the download could not be started" for `id` and whether an event should be
/// surfaced now: the first failure of a streak and then a reminder every `remind_every`
/// failures, so a stuck item is visible without an event every frame.
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
fn record_download_failure(failures: &Mutex<BTreeMap<u64, u32>>, id: u64, remind_every: u32) -> bool {
    let mut map = failures.lock().unwrap_or_else(|e| e.into_inner());
    let streak = map.entry(id).or_insert(0);
    *streak += 1;
    *streak == 1 || (*streak).is_multiple_of(remind_every)
}

/// Clears the failure streak of `id`: `download_item` accepted the request, so a later refusal
/// starts a fresh streak instead of continuing the old count (corr-low, ADR-5789).
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
fn clear_download_failures(failures: &Mutex<BTreeMap<u64, u32>>, id: u64) {
    let mut map = failures.lock().unwrap_or_else(|e| e.into_inner());
    map.remove(&id);
}

// ------------------------------------------------------------------------------------------------
// The mock

/// One item of the mock Workshop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockWorkshopItem {
    /// Title.
    pub title: String,
    /// Content: relative path → bytes.
    pub files: BTreeMap<String, Vec<u8>>,
    /// Tags.
    pub tags: Vec<String>,
    /// Visibility.
    pub visibility: WorkshopVisibility,
    /// Metadata.
    pub metadata: String,
    /// Uploads that changed it.
    pub revision: u32,
}

#[derive(Debug, Default)]
struct MockState {
    next_item: u64,
    next_request: u64,
    items: BTreeMap<u64, MockWorkshopItem>,
    subscribed: BTreeSet<u64>,
    installed: BTreeMap<u64, (InstalledItem, u32)>,
    pending_installs: BTreeSet<u64>,
    events: Vec<WorkshopEvent>,
    uploads: Vec<WorkshopUpload>,
    fail_next: Option<String>,
    installs: u32,
    /// Unique path counter for staging/aside folders (an interrupted install never collides
    /// with the next attempt's).
    attempts: u64,
    /// Test hook: the next N installs fail as a transient Steam download error would.
    fail_installs: u32,
}

/// An in-memory Workshop in the pattern of [`crate::MockSteam`]: share it between the test and the
/// plugin ([`SteamWorkshopPlugin::with_service`]), drive frames, and inspect what was uploaded.
/// Uploads read the content folder; subscribing "downloads" an item into
/// `<install_root>/<item id>/` on the next [`WorkshopService::poll`] (written to a temporary
/// folder and renamed, as a finished download appears).
#[derive(Debug)]
pub struct MockWorkshop {
    install_root: PathBuf,
    state: Mutex<MockState>,
}

impl MockWorkshop {
    /// A shared mock that installs items under `install_root`. Item ids start at 1,000,000.
    pub fn shared(install_root: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            install_root: install_root.into(),
            state: Mutex::new(MockState {
                next_item: 1_000_000,
                next_request: 1,
                ..MockState::default()
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Puts an item on the mock Workshop as if another player had uploaded it. Paths that are not
    /// valid `mods://` paths are dropped, as a Workshop item can hold only files below its folder.
    pub fn publish(&self, title: &str, files: BTreeMap<String, Vec<u8>>) -> WorkshopItemId {
        let files = safe_files(files);
        let mut state = self.lock();
        let id = state.next_item;
        state.next_item += 1;
        state.items.insert(
            id,
            MockWorkshopItem {
                title: title.to_owned(),
                files,
                tags: Vec::new(),
                visibility: WorkshopVisibility::Public,
                metadata: String::new(),
                revision: 1,
            },
        );
        WorkshopItemId(id)
    }

    /// Replaces an item's files as its author's update would; a subscribed item is installed
    /// again on the next poll.
    pub fn update_item(&self, item: WorkshopItemId, files: BTreeMap<String, Vec<u8>>) -> bool {
        let mut state = self.lock();
        let Some(entry) = state.items.get_mut(&item.0) else {
            return false;
        };
        entry.files = safe_files(files);
        entry.revision += 1;
        if state.subscribed.contains(&item.0) {
            state.pending_installs.insert(item.0);
        }
        true
    }

    /// An item as the mock Workshop holds it.
    #[must_use]
    pub fn item(&self, item: WorkshopItemId) -> Option<MockWorkshopItem> {
        self.lock().items.get(&item.0).cloned()
    }

    /// Every upload the game asked for, in order.
    #[must_use]
    pub fn uploads(&self) -> Vec<WorkshopUpload> {
        self.lock().uploads.clone()
    }

    /// Makes the next upload or subscribe fail with `reason` (as Steam would report it).
    pub fn fail_next(&self, reason: &str) {
        self.lock().fail_next = Some(reason.to_owned());
    }

    /// Makes the next `count` installs fail (as a transient Steam download error would), so a
    /// test can watch the poll retry them.
    pub fn fail_installs(&self, count: u32) {
        self.lock().fail_installs = count;
    }

    /// Installs written so far (updates included).
    #[must_use]
    pub fn installs(&self) -> u32 {
        self.lock().installs
    }

    fn install(&self, state: &mut MockState, id: u64) -> Result<InstalledItem, String> {
        if state.fail_installs > 0 {
            state.fail_installs -= 1;
            return Err("injected install failure".to_owned());
        }
        let item = state.items.get(&id).ok_or_else(|| format!("item {id} is gone"))?;
        let folder = self.install_root.join(id.to_string());
        state.attempts += 1;
        // Best-effort sweep of an earlier update's trash: a reader still holding the old
        // install open (a mounted pak) makes the post-swap removal fail on Windows, and each
        // attempt's trash name is unique, so without the sweep every such update orphans a
        // `.trash-<id>-<n>` folder (review of fix-steam-platform, finding 4).
        sweep_trash(&self.install_root, id);
        let staging = self
            .install_root
            .join(format!(".staging-{id}-{}", state.attempts));
        let trash = self.install_root.join(format!(".trash-{id}-{}", state.attempts));
        install_tree(&item.files, &staging, &trash, &folder)
            .map_err(|e| format!("installing item {id} into {}: {e}", folder.display()))?;
        let size_on_disk = item.files.values().map(|b| b.len() as u64).sum();
        let revision = item.revision;
        state.installs += 1;
        let installed = InstalledItem {
            folder,
            size_on_disk,
            timestamp: revision,
        };
        state.installed.insert(id, (installed.clone(), revision));
        Ok(installed)
    }
}

/// `files` without the paths that are not valid, non-empty `mods://` paths.
fn safe_files(files: BTreeMap<String, Vec<u8>>) -> BTreeMap<String, Vec<u8>> {
    files
        .into_iter()
        .filter_map(|(path, bytes)| match regia_asset::mods_vfs::mods_relative(&path) {
            Ok(rel) if !rel.is_empty() => Some((rel, bytes)),
            _ => {
                tracing::warn!(target: "regia_steam", path = %path, "mock Workshop: a path outside the item was dropped");
                None
            }
        })
        .collect()
}

/// Best-effort removal of `install_root`'s stale `.trash-<id>-*` folders: an earlier update
/// whose post-swap trash removal failed (a reader held the old install open) leaves one, and
/// nothing else sweeps them. Failures are ignored: the sweep runs before the next swap, which
/// is the recovery path that matters.
fn sweep_trash(install_root: &Path, id: u64) {
    let Ok(entries) = std::fs::read_dir(install_root) else {
        return;
    };
    let prefix = format!(".trash-{id}-");
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Writes `files` into `staging`, then swaps the staged tree over `folder` through
/// [`replace_tree`]: a reader sees the old install or the whole new one, never a missing folder,
/// even when the swap fails. The old install is renamed aside, not deleted, before the new one
/// lands (Windows cannot delete a folder while a file inside it is open). `staging` is removed when
/// the install fails; the caller makes `staging` and `trash` unique per install attempt.
///
/// An empty `files` map is refused before anything is created or renamed. Callers drop paths
/// that are not valid `mods://` paths, and installing that empty remainder would replace a good
/// tree with an empty directory and report the install as successful.
fn install_tree(
    files: &BTreeMap<String, Vec<u8>>,
    staging: &Path,
    trash: &Path,
    folder: &Path,
) -> std::io::Result<()> {
    if files.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to install an empty file set over the item",
        ));
    }
    fn write_files(files: &BTreeMap<String, Vec<u8>>, staging: &Path) -> std::io::Result<()> {
        if staging.exists() {
            std::fs::remove_dir_all(staging)?;
        }
        for (rel, bytes) in files {
            let path = staging.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, bytes)?;
        }
        std::fs::create_dir_all(staging)?;
        Ok(())
    }
    let result = write_files(files, staging).and_then(|()| replace_tree(staging, folder, trash));
    if result.is_err() {
        let _ = std::fs::remove_dir_all(staging);
    }
    result
}

/// Swaps `staging` over `folder`, keeping the previous install reachable throughout: the live
/// folder is renamed to `aside` first and renamed back when the second rename fails, so a failed
/// swap — or a stop between the two renames — leaves the old install in place. Dropping the old
/// copy after a successful swap is best-effort: a leftover `aside` does not affect the installed
/// tree and is removed by the next install's fresh paths.
///
/// Windows refuses to rename a folder while a file inside it is open (a reader holding the
/// mounted install), so when the live folder cannot move aside the swap moves the files one at a
/// time instead ([`replace_entries`], ADR-8504): an open file can be renamed, its folder cannot.
fn replace_tree(staging: &Path, folder: &Path, aside: &Path) -> std::io::Result<()> {
    if folder.exists()
        && let Err(whole) = std::fs::rename(folder, aside)
    {
        tracing::debug!(
            target: "regia_steam",
            folder = %folder.display(),
            error = %whole,
            "mock Workshop: the install folder cannot move aside; swapping its files one at a time"
        );
        return replace_entries(staging, folder, aside);
    }
    if let Err(err) = std::fs::rename(staging, folder) {
        // The staged tree could not take the live path: put the previous install back.
        if aside.exists() {
            let _ = std::fs::rename(aside, folder);
        }
        return Err(err);
    }
    if aside.exists() {
        let _ = std::fs::remove_dir_all(aside);
    }
    Ok(())
}

/// The swap of [`replace_tree`] when the live `folder` itself cannot be renamed (ADR-8504): the
/// folder stays where it is, every file of the old install moves into `aside`, then every file of
/// `staging` moves into `folder`. A failure at any step moves what already moved back (the new
/// files to `staging`, the old ones to `folder`), so the previous install stays complete; `aside`
/// is kept when that undo is itself refused, so no old file is ever deleted before the new tree is
/// whole. On success the emptied `staging` and the old files in `aside` are removed, best-effort.
///
/// Unlike the folder rename, a reader looking at `folder` during the swap can see it part-empty or
/// with some new files; it never sees a missing folder. A Workshop update lands in place the same
/// way.
fn replace_entries(staging: &Path, folder: &Path, aside: &Path) -> std::io::Result<()> {
    let old = tree_files(folder)?;
    let new = tree_files(staging)?;
    std::fs::create_dir_all(aside)?;
    if let Err((moved, err)) = move_files(&old, folder, aside) {
        if move_files(&old[..moved], aside, folder).is_ok() {
            let _ = std::fs::remove_dir_all(aside);
        }
        return Err(err);
    }
    remove_empty_dirs(folder);
    if let Err((moved, err)) = move_files(&new, staging, folder) {
        let _ = move_files(&new[..moved], folder, staging);
        remove_empty_dirs(folder);
        if move_files(&old, aside, folder).is_ok() {
            let _ = std::fs::remove_dir_all(aside);
        }
        return Err(err);
    }
    let _ = std::fs::remove_dir_all(staging);
    let _ = std::fs::remove_dir_all(aside);
    Ok(())
}

/// Every file (anything that is not a folder) below `dir`, relative to it, in sorted order.
fn tree_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut folders = vec![PathBuf::new()];
    while let Some(rel) = folders.pop() {
        for entry in std::fs::read_dir(dir.join(&rel))? {
            let entry = entry?;
            let path = rel.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                folders.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Renames each of `files` from below `from` to the same place below `to`, creating folders as
/// needed. On a failure answers how many files had moved and the error.
fn move_files(files: &[PathBuf], from: &Path, to: &Path) -> Result<(), (usize, std::io::Error)> {
    for (moved, rel) in files.iter().enumerate() {
        let target = to.join(rel);
        let result = target
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::rename(from.join(rel), &target));
        if let Err(err) = result {
            return Err((moved, err));
        }
    }
    Ok(())
}

/// Removes the empty folders below `dir`, deepest first, keeping `dir` itself. Best-effort: a
/// folder that is not empty or cannot be removed stays.
fn remove_empty_dirs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            let path = entry.path();
            remove_empty_dirs(&path);
            let _ = std::fs::remove_dir(&path);
        }
    }
}

/// Reads every file under `dir` through a `mods://` mount (confined, no links).
fn read_tree(dir: &Path) -> Result<BTreeMap<String, Vec<u8>>, WorkshopError> {
    let mut vfs = ModsVfs::new();
    vfs.mount("upload", ModSource::Dir(dir.to_path_buf()))
        .map_err(WorkshopError::Content)?;
    vfs.mod_files("upload").map_err(WorkshopError::Content)
}

impl WorkshopService for MockWorkshop {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn upload(&self, upload: &WorkshopUpload, metadata: &str) -> Result<WorkshopRequest, WorkshopError> {
        let files = read_tree(&upload.content_dir)?;
        let mut state = self.lock();
        let request = WorkshopRequest(state.next_request);
        state.next_request += 1;
        state.uploads.push(upload.clone());
        if let Some(reason) = state.fail_next.take() {
            state.events.push(WorkshopEvent::UploadFailed {
                request,
                error: WorkshopError::Platform(reason),
            });
            return Ok(request);
        }
        let id = match upload.item {
            Some(item) if state.items.contains_key(&item.0) => item.0,
            Some(item) => {
                state.events.push(WorkshopEvent::UploadFailed {
                    request,
                    error: WorkshopError::UnknownItem(item.0),
                });
                return Ok(request);
            }
            None => {
                let id = state.next_item;
                state.next_item += 1;
                id
            }
        };
        let revision = state.items.get(&id).map_or(1, |i| i.revision + 1);
        state.items.insert(
            id,
            MockWorkshopItem {
                title: upload.title.clone(),
                files,
                tags: upload.tags.clone(),
                visibility: upload.visibility,
                metadata: metadata.to_owned(),
                revision,
            },
        );
        if state.subscribed.contains(&id) {
            state.pending_installs.insert(id);
        }
        state.events.push(WorkshopEvent::Uploaded {
            request,
            item: WorkshopItemId(id),
            needs_legal_agreement: false,
        });
        Ok(request)
    }

    fn subscribe(&self, item: WorkshopItemId) -> Result<(), WorkshopError> {
        let mut state = self.lock();
        if let Some(reason) = state.fail_next.take() {
            state.events.push(WorkshopEvent::SubscribeFailed {
                item,
                error: WorkshopError::Platform(reason),
            });
            return Ok(());
        }
        if !state.items.contains_key(&item.0) {
            state.events.push(WorkshopEvent::SubscribeFailed {
                item,
                error: WorkshopError::UnknownItem(item.0),
            });
            return Ok(());
        }
        if state.subscribed.insert(item.0) {
            state.events.push(WorkshopEvent::Subscribed { item });
            state.pending_installs.insert(item.0);
        }
        Ok(())
    }

    fn unsubscribe(&self, item: WorkshopItemId) -> Result<(), WorkshopError> {
        let mut state = self.lock();
        // Same check the real client uses, so the helper is part of the default build.
        unsubscribe_if_subscribed(item, state.subscribed.contains(&item.0))?;
        state.subscribed.remove(&item.0);
        state.pending_installs.remove(&item.0);
        if let Some((installed, _)) = state.installed.remove(&item.0) {
            let _ = std::fs::remove_dir_all(&installed.folder);
        }
        state.events.push(WorkshopEvent::Unsubscribed { item });
        Ok(())
    }

    fn subscribed(&self) -> Vec<WorkshopItemId> {
        self.lock()
            .subscribed
            .iter()
            .map(|&id| WorkshopItemId(id))
            .collect()
    }

    fn item_state(&self, item: WorkshopItemId) -> WorkshopItemState {
        let state = self.lock();
        let revision = state.items.get(&item.0).map(|i| i.revision);
        let installed = state.installed.get(&item.0).map(|(_, r)| *r);
        WorkshopItemState {
            subscribed: state.subscribed.contains(&item.0),
            installed: installed.is_some(),
            downloading: state.pending_installs.contains(&item.0),
            needs_update: installed.is_some() && installed != revision,
        }
    }

    fn install_info(&self, item: WorkshopItemId) -> Option<InstalledItem> {
        self.lock().installed.get(&item.0).map(|(i, _)| i.clone())
    }

    fn poll(&self) -> Vec<WorkshopEvent> {
        let mut state = self.lock();
        let pending: Vec<u64> = std::mem::take(&mut state.pending_installs).into_iter().collect();
        for id in pending {
            let item = WorkshopItemId(id);
            match self.install(&mut state, id) {
                Ok(installed) => state.events.push(WorkshopEvent::Installed {
                    item,
                    folder: installed.folder,
                }),
                Err(message) => {
                    // A transient failure (IO, a busy disk) must not strand the item: while it
                    // is still on the Workshop, the next poll installs it again (`install_tree`
                    // keeps the previous folder in place). An item that is gone is not retried.
                    if state.items.contains_key(&id) {
                        state.pending_installs.insert(id);
                    }
                    state.events.push(WorkshopEvent::SubscribeFailed {
                        item,
                        error: WorkshopError::Platform(message),
                    });
                }
            }
        }
        std::mem::take(&mut state.events)
    }
}

/// Polls between full scans of the subscribed library (about two seconds at 60 fps).
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
pub(crate) const SCAN_EVERY_POLLS: u64 = 120;
/// Polls between queries of one item that is still downloading. Not every frame.
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
pub(crate) const WAITING_QUERY_EVERY: u64 = 30;

/// What one `poll` should ask the Workshop service.
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PollQuery {
    /// Walk every subscribed item.
    pub scan: bool,
    /// Ask `item_state` for ids that are still downloading.
    pub query_waiting: bool,
    /// Re-read the install info (timestamp) of known stable items too: an update that started
    /// and finished between two scans never shows its downloading state bits.
    pub recheck_installed: bool,
}

/// Scans between re-reads of every known install's timestamp (about half a minute at 60 fps).
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
pub(crate) const RECHECK_EVERY_SCANS: u64 = 16;

/// Packs the four state flags `known` compares so an unchanged install skips `install_info`.
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
pub(crate) fn pack_state(state: WorkshopItemState) -> u32 {
    let mut bits = 0u32;
    if state.subscribed {
        bits |= 1;
    }
    if state.installed {
        bits |= 2;
    }
    if state.downloading {
        bits |= 4;
    }
    if state.needs_update {
        bits |= 8;
    }
    bits
}

/// Records the state bits a poll saw for an already-known item that is not a stable install
/// (an update is downloading or pending). Without this the bits in `known` stay those of the
/// last stable read, so when the update finishes the state reads "unchanged" and the new install
/// is never reported. Unknown ids are left alone (their first stable sight is read anyway).
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
pub(crate) fn note_unstable_state(known: &mut BTreeMap<u64, (u32, u32)>, id: u64, bits: u32) {
    if let Some(entry) = known.get_mut(&id) {
        entry.1 = bits;
    }
}

/// The questions poll number `poll_index` (the count before this poll) should ask.
///
/// A full scan of the subscribed library runs every [`SCAN_EVERY_POLLS`] polls. An item that is
/// still downloading is asked every [`WAITING_QUERY_EVERY`] polls, not every frame.
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
pub(crate) fn poll_query(poll_index: u64) -> PollQuery {
    PollQuery {
        scan: poll_index.is_multiple_of(SCAN_EVERY_POLLS),
        query_waiting: poll_index.is_multiple_of(WAITING_QUERY_EVERY),
        recheck_installed: poll_index.is_multiple_of(SCAN_EVERY_POLLS * RECHECK_EVERY_SCANS),
    }
}

/// Whether a scan should call `install_info` for `state`.
///
/// A stable install that is already `known` and whose state bits did not change is skipped.
/// The first sight of an install, and any later change of those bits, is read.
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
pub(crate) fn should_read_install_info(known: bool, state_unchanged: bool, state: WorkshopItemState) -> bool {
    if !state.installed || state.downloading || state.needs_update {
        return false;
    }
    !known || !state_unchanged
}

// ------------------------------------------------------------------------------------------------
// The real Workshop (ISteamUGC over the steamworks 0.13 bindings)

#[cfg(all(feature = "steamworks", not(target_os = "android")))]
pub use real::RealWorkshop;

#[cfg(all(feature = "steamworks", not(target_os = "android")))]
mod real {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use super::{
        InstalledItem, WorkshopError, WorkshopEvent, WorkshopItemId, WorkshopItemState, WorkshopRequest,
        WorkshopService, WorkshopUpload, WorkshopVisibility, clear_download_failures,
        record_download_failure, unsubscribe_if_subscribed,
    };

    /// State the call-result callbacks share.
    #[derive(Default)]
    struct Shared {
        events: Mutex<Vec<WorkshopEvent>>,
        /// Subscribed items whose install the poll is waiting for.
        waiting: Mutex<BTreeSet<u64>>,
        /// The install time stamp and packed state bits last reported per item. A newer stamp is
        /// an update to report again; unchanged bits skip `install_info`. An item gone from the
        /// subscribed list was unsubscribed (through the overlay, say).
        known: Mutex<BTreeMap<u64, (u32, u32)>>,
        /// Items a download was asked for, so it is asked once, not every frame.
        requested: Mutex<BTreeSet<u64>>,
        /// Consecutive "download could not be started" counts per item (`record_download_failure`).
        download_failures: Mutex<BTreeMap<u64, u32>>,
    }

    fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    impl Shared {
        fn push(&self, event: WorkshopEvent) {
            self.events.lock().unwrap_or_else(|e| e.into_inner()).push(event);
        }
        fn wait_for(&self, item: u64) {
            self.waiting
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(item);
        }
    }

    fn platform(e: steamworks::SteamError) -> WorkshopError {
        WorkshopError::Platform(e.to_string())
    }

    fn visibility(v: WorkshopVisibility) -> steamworks::PublishedFileVisibility {
        match v {
            WorkshopVisibility::Public => steamworks::PublishedFileVisibility::Public,
            WorkshopVisibility::FriendsOnly => steamworks::PublishedFileVisibility::FriendsOnly,
            WorkshopVisibility::Private => steamworks::PublishedFileVisibility::Private,
            WorkshopVisibility::Unlisted => steamworks::PublishedFileVisibility::Unlisted,
        }
    }

    /// The Steam Workshop of one connection (ISteamUGC). Every call registers a Steam call result;
    /// its callback runs inside `run_callbacks` (the plugin's frame pump) and queues an event that
    /// [`WorkshopService::poll`] hands out. Installs are noticed by polling the item state of
    /// subscribed items, which also covers items Steam installed before the game started.
    pub struct RealWorkshop {
        client: steamworks::Client,
        shared: Arc<Shared>,
        next: AtomicU64,
        polls: AtomicU64,
    }

    impl RealWorkshop {
        /// The Workshop of `client` (the engine's shared client, `Steam::client`).
        #[must_use]
        pub fn new(client: steamworks::Client) -> Self {
            Self {
                client,
                shared: Arc::new(Shared::default()),
                next: AtomicU64::new(1),
                polls: AtomicU64::new(0),
            }
        }
    }

    /// One update to fill in and submit.
    struct Submit {
        app: steamworks::AppId,
        item: steamworks::PublishedFileId,
        upload: WorkshopUpload,
        metadata: String,
        request: WorkshopRequest,
        shared: Arc<Shared>,
        /// The create step said the user must accept the legal agreement.
        legal_from_create: bool,
    }

    /// Fills in an update and submits it; the result arrives as an event.
    fn submit(client: &steamworks::Client, job: Submit) {
        let Submit {
            app,
            item,
            upload,
            metadata,
            request,
            shared,
            legal_from_create,
        } = job;
        let mut update = client
            .ugc()
            .start_item_update(app, item)
            .title(&upload.title)
            .description(&upload.description)
            .content_path(&upload.content_dir)
            .visibility(visibility(upload.visibility))
            .tags(upload.tags.clone(), false);
        if !metadata.is_empty() {
            update = update.metadata(&metadata);
        }
        if let Some(preview) = &upload.preview {
            update = update.preview_path(preview);
        }
        let note = (!upload.change_note.is_empty()).then_some(upload.change_note.as_str());
        // The watch handle only reports progress; dropping it does not cancel the upload.
        let _watch = update.submit(note, move |result| {
            shared.push(match result {
                Ok((id, legal)) => WorkshopEvent::Uploaded {
                    request,
                    item: WorkshopItemId(id.0),
                    needs_legal_agreement: legal || legal_from_create,
                },
                Err(e) => WorkshopEvent::UploadFailed {
                    request,
                    error: platform(e),
                },
            });
        });
    }

    impl WorkshopService for RealWorkshop {
        fn name(&self) -> &'static str {
            "steam"
        }

        fn upload(&self, upload: &WorkshopUpload, metadata: &str) -> Result<WorkshopRequest, WorkshopError> {
            let request = WorkshopRequest(self.next.fetch_add(1, Ordering::Relaxed));
            let app = self.client.utils().app_id();
            let shared = Arc::clone(&self.shared);
            let job = |item, legal_from_create, shared| Submit {
                app,
                item,
                upload: upload.clone(),
                metadata: metadata.to_owned(),
                request,
                shared,
                legal_from_create,
            };
            match upload.item {
                Some(item) => submit(
                    &self.client,
                    job(steamworks::PublishedFileId(item.0), false, shared),
                ),
                None => {
                    let client = self.client.clone();
                    let mut pending = job(steamworks::PublishedFileId(0), false, Arc::clone(&shared));
                    self.client
                        .ugc()
                        .create_item(app, steamworks::FileType::Community, move |result| match result {
                            Ok((id, legal)) => {
                                pending.item = id;
                                pending.legal_from_create = legal;
                                submit(&client, pending);
                            }
                            Err(e) => shared.push(WorkshopEvent::UploadFailed {
                                request,
                                error: platform(e),
                            }),
                        });
                }
            }
            Ok(request)
        }

        fn subscribe(&self, item: WorkshopItemId) -> Result<(), WorkshopError> {
            let shared = Arc::clone(&self.shared);
            self.client.ugc().subscribe_item(
                steamworks::PublishedFileId(item.0),
                move |result| match result {
                    Ok(()) => {
                        shared.wait_for(item.0);
                        shared.push(WorkshopEvent::Subscribed { item });
                    }
                    Err(e) => shared.push(WorkshopEvent::SubscribeFailed {
                        item,
                        error: platform(e),
                    }),
                },
            );
            Ok(())
        }

        fn unsubscribe(&self, item: WorkshopItemId) -> Result<(), WorkshopError> {
            unsubscribe_if_subscribed(item, self.item_state(item).subscribed)?;
            let shared = Arc::clone(&self.shared);
            locked(&self.shared.waiting).remove(&item.0);
            locked(&self.shared.known).remove(&item.0);
            locked(&self.shared.requested).remove(&item.0);
            locked(&self.shared.download_failures).remove(&item.0);
            self.client.ugc().unsubscribe_item(
                steamworks::PublishedFileId(item.0),
                move |result| match result {
                    Ok(()) => shared.push(WorkshopEvent::Unsubscribed { item }),
                    Err(e) => shared.push(WorkshopEvent::UnsubscribeFailed {
                        item,
                        error: platform(e),
                    }),
                },
            );
            Ok(())
        }

        fn subscribed(&self) -> Vec<WorkshopItemId> {
            let mut items: Vec<WorkshopItemId> = self
                .client
                .ugc()
                .subscribed_items(false)
                .into_iter()
                .map(|id| WorkshopItemId(id.0))
                .collect();
            items.sort_unstable();
            items
        }

        fn item_state(&self, item: WorkshopItemId) -> WorkshopItemState {
            let state = self.client.ugc().item_state(steamworks::PublishedFileId(item.0));
            WorkshopItemState {
                subscribed: state.contains(steamworks::ItemState::SUBSCRIBED),
                installed: state.contains(steamworks::ItemState::INSTALLED),
                downloading: state
                    .intersects(steamworks::ItemState::DOWNLOADING | steamworks::ItemState::DOWNLOAD_PENDING),
                needs_update: state.contains(steamworks::ItemState::NEEDS_UPDATE),
            }
        }

        fn install_info(&self, item: WorkshopItemId) -> Option<InstalledItem> {
            let info = self
                .client
                .ugc()
                .item_install_info(steamworks::PublishedFileId(item.0))?;
            // The caller now knows this install; a scan reports only a newer one.
            locked(&self.shared.known)
                .insert(item.0, (info.timestamp, super::pack_state(self.item_state(item))));
            Some(InstalledItem {
                folder: PathBuf::from(info.folder),
                size_on_disk: info.size_on_disk,
                timestamp: info.timestamp,
            })
        }

        fn poll(&self) -> Vec<WorkshopEvent> {
            let mut events = std::mem::take(&mut *locked(&self.shared.events));
            let query = super::poll_query(self.polls.fetch_add(1, Ordering::Relaxed));
            let mut candidates: BTreeSet<u64> = if query.query_waiting {
                locked(&self.shared.waiting).clone()
            } else {
                BTreeSet::new()
            };
            if query.scan {
                let subscribed: BTreeSet<u64> = self.subscribed().into_iter().map(|i| i.0).collect();
                // Unsubscribed elsewhere (the overlay, the Workshop page).
                let gone: Vec<u64> = locked(&self.shared.known)
                    .keys()
                    .filter(|id| !subscribed.contains(id))
                    .copied()
                    .collect();
                for id in gone {
                    locked(&self.shared.known).remove(&id);
                    locked(&self.shared.requested).remove(&id);
                    locked(&self.shared.download_failures).remove(&id);
                    events.push(WorkshopEvent::Unsubscribed {
                        item: WorkshopItemId(id),
                    });
                }
                candidates.extend(subscribed);
            }
            for id in candidates {
                let item = WorkshopItemId(id);
                let state = self.item_state(item);
                let known_entry = locked(&self.shared.known).get(&id).copied();
                let bits = super::pack_state(state);
                let known = known_entry.is_some();
                let unchanged = known_entry.is_some_and(|(_, prev)| prev == bits);
                let stable = state.installed && !state.downloading && !state.needs_update;
                if !stable {
                    super::note_unstable_state(&mut locked(&self.shared.known), id, bits);
                }
                if stable {
                    if query.recheck_installed || super::should_read_install_info(known, unchanged, state) {
                        let before_ts = known_entry.map(|(timestamp, _)| timestamp);
                        if let Some(info) = self.install_info(item) {
                            locked(&self.shared.waiting).remove(&id);
                            locked(&self.shared.requested).remove(&id);
                            locked(&self.shared.download_failures).remove(&id);
                            if before_ts != Some(info.timestamp) {
                                events.push(WorkshopEvent::Installed {
                                    item,
                                    folder: info.folder,
                                });
                            }
                        }
                    } else {
                        // Known stable install: stay mounted, do not stat the folder again.
                        locked(&self.shared.waiting).remove(&id);
                        locked(&self.shared.requested).remove(&id);
                        locked(&self.shared.download_failures).remove(&id);
                    }
                } else if !state.downloading && !locked(&self.shared.requested).contains(&id) {
                    // `download_item` also returns false when the item is already current; the
                    // installed-state branch above covers that, so a false here means the
                    // download could not be started: leave the id out of `requested` so the next
                    // poll asks again, and surface the failure once per streak instead of
                    // swallowing it (the item would else stay uninstalled until a restart).
                    if self
                        .client
                        .ugc()
                        .download_item(steamworks::PublishedFileId(id), true)
                    {
                        // Asked once per install or update, not every frame.
                        locked(&self.shared.requested).insert(id);
                        clear_download_failures(&self.shared.download_failures, id);
                    } else if record_download_failure(
                        &self.shared.download_failures,
                        id,
                        super::SCAN_EVERY_POLLS as u32,
                    ) {
                        events.push(WorkshopEvent::DownloadFailed {
                            item,
                            error: WorkshopError::Platform("Steam could not start the item download".into()),
                        });
                    }
                }
            }
            events
        }
    }
}

// ------------------------------------------------------------------------------------------------
// The resource and the plugin

/// Where an upload stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadStatus {
    /// Sent; no answer yet.
    Pending,
    /// Accepted.
    Done {
        /// The item.
        item: WorkshopItemId,
        /// The user must accept the Workshop legal agreement first.
        needs_legal_agreement: bool,
    },
    /// Failed.
    Failed(WorkshopError),
}

/// One subscribed (or once subscribed) item as the game sees it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkshopItemStatus {
    /// Subscribed now.
    pub subscribed: bool,
    /// Its install folder, once installed.
    pub folder: Option<PathBuf>,
    /// The `mods://` mod id it is mounted as, while mounted.
    pub mounted: Option<String>,
    /// Installs seen (an update is one more).
    pub installs: u32,
    /// The last subscribe, download, unsubscribe or mount failure.
    pub error: Option<String>,
}

/// Most events [`SteamWorkshop::recent`] keeps.
pub const MAX_RECENT_EVENTS: usize = 64;

/// The Workshop as the game uses it (resource, added by [`SteamWorkshopPlugin`]).
#[derive(Resource)]
pub struct SteamWorkshop {
    service: Arc<dyn WorkshopService>,
    injected: bool,
    auto_mount: bool,
    agent_upload: bool,
    items: BTreeMap<WorkshopItemId, WorkshopItemStatus>,
    uploads: BTreeMap<WorkshopRequest, UploadStatus>,
    recent: VecDeque<WorkshopEvent>,
    adopted: bool,
}

impl std::fmt::Debug for SteamWorkshop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SteamWorkshop")
            .field("service", &self.service.name())
            .field("items", &self.items)
            .field("uploads", &self.uploads)
            .finish_non_exhaustive()
    }
}

impl SteamWorkshop {
    /// A Workshop over `service`.
    #[must_use]
    pub fn new(service: Arc<dyn WorkshopService>) -> Self {
        Self {
            service,
            injected: true,
            auto_mount: true,
            agent_upload: false,
            items: BTreeMap::new(),
            uploads: BTreeMap::new(),
            recent: VecDeque::new(),
            adopted: false,
        }
    }

    fn unattached() -> Self {
        Self {
            injected: false,
            ..Self::new(Arc::new(UnavailableWorkshop))
        }
    }

    /// The service's name: `steam`, `mock` or `unavailable`.
    #[must_use]
    pub fn service_name(&self) -> &'static str {
        self.service.name()
    }

    /// Whether installed items mount into `mods://` by themselves (default on).
    #[must_use]
    pub fn auto_mount(&self) -> bool {
        self.auto_mount
    }

    /// Whether the agent may upload to this service: always for the mock, and for the real
    /// Workshop only when the game opted in ([`SteamWorkshopPlugin::allow_agent_upload`]).
    #[must_use]
    pub fn agent_upload_allowed(&self) -> bool {
        self.agent_upload || self.service.name() == "mock"
    }

    /// Starts an upload of `upload` (checked first; nothing is sent when the check fails). The
    /// checked copy is sent: its content and preview paths are absolute.
    pub fn upload(&mut self, upload: &WorkshopUpload) -> Result<WorkshopRequest, WorkshopError> {
        let mut upload = upload.clone();
        let checked = upload.check()?;
        let metadata = upload.metadata_for(&checked);
        let request = self.service.upload(&upload, &metadata)?;
        self.uploads.insert(request, UploadStatus::Pending);
        Ok(request)
    }

    /// Subscribes to `item`; it mounts once installed.
    pub fn subscribe(&mut self, item: WorkshopItemId) -> Result<(), WorkshopError> {
        self.service.subscribe(item)?;
        self.items.entry(item).or_default().error = None;
        Ok(())
    }

    /// Unsubscribes from `item`; it unmounts when Steam confirms.
    pub fn unsubscribe(&mut self, item: WorkshopItemId) -> Result<(), WorkshopError> {
        self.service.unsubscribe(item)
    }

    /// The items seen, by id.
    #[must_use]
    pub fn items(&self) -> &BTreeMap<WorkshopItemId, WorkshopItemStatus> {
        &self.items
    }

    /// The uploads sent, by request.
    #[must_use]
    pub fn uploads(&self) -> &BTreeMap<WorkshopRequest, UploadStatus> {
        &self.uploads
    }

    /// The service's view of an item.
    #[must_use]
    pub fn item_state(&self, item: WorkshopItemId) -> WorkshopItemState {
        self.service.item_state(item)
    }

    /// The last [`MAX_RECENT_EVENTS`] events, oldest first.
    pub fn recent(&self) -> impl Iterator<Item = &WorkshopEvent> {
        self.recent.iter()
    }

    /// Polls the service and applies what finished to `mods` (mounts installed items, unmounts
    /// unsubscribed ones). The plugin's `PreUpdate` step calls this once per frame; tests and
    /// hosts without the plugin may call it themselves.
    pub fn update(&mut self, mut mods: Option<&mut ModsVfs>) -> Vec<WorkshopEvent> {
        let _span = tracing::trace_span!("steam_workshop.update").entered();
        let mut events = Vec::new();
        if !self.adopted {
            // Items installed before this session (Steam keeps them between runs).
            self.adopted = true;
            for item in self.service.subscribed() {
                self.items.entry(item).or_default().subscribed = true;
                // Only a finished install: one Steam is still downloading or updating arrives as an
                // `Installed` event from the service once it is done.
                let state = self.service.item_state(item);
                if !state.installed || state.downloading || state.needs_update {
                    continue;
                }
                if let Some(info) = self.service.install_info(item) {
                    events.push(WorkshopEvent::Installed {
                        item,
                        folder: info.folder,
                    });
                }
            }
        }
        events.extend(self.service.poll());
        for event in &events {
            self.apply(event, mods.as_deref_mut());
            if self.recent.len() == MAX_RECENT_EVENTS {
                self.recent.pop_front();
            }
            self.recent.push_back(event.clone());
        }
        events
    }

    fn apply(&mut self, event: &WorkshopEvent, mods: Option<&mut ModsVfs>) {
        match event {
            WorkshopEvent::Uploaded {
                request,
                item,
                needs_legal_agreement,
            } => {
                self.uploads.insert(
                    *request,
                    UploadStatus::Done {
                        item: *item,
                        needs_legal_agreement: *needs_legal_agreement,
                    },
                );
            }
            WorkshopEvent::UploadFailed { request, error } => {
                tracing::warn!(target: "regia_steam", request = request.0, "Workshop upload failed: {error}");
                self.uploads.insert(*request, UploadStatus::Failed(error.clone()));
            }
            WorkshopEvent::Subscribed { item } => {
                self.items.entry(*item).or_default().subscribed = true;
            }
            WorkshopEvent::SubscribeFailed { item, error } => {
                tracing::warn!(target: "regia_steam", item = item.0, "Workshop subscribe failed: {error}");
                self.items.entry(*item).or_default().error = Some(error.to_string());
            }
            WorkshopEvent::UnsubscribeFailed { item, error } => {
                tracing::warn!(target: "regia_steam", item = item.0, "Workshop unsubscribe failed: {error}");
                let status = self.items.entry(*item).or_default();
                status.subscribed = true;
                status.error = Some(format!("unsubscribe failed: {error}"));
            }
            WorkshopEvent::DownloadFailed { item, error } => {
                tracing::warn!(target: "regia_steam", item = item.0, "Workshop download failed: {error}");
                self.items.entry(*item).or_default().error = Some(error.to_string());
            }
            WorkshopEvent::Unsubscribed { item } => {
                let status = self.items.entry(*item).or_default();
                status.subscribed = false;
                status.folder = None;
                if let Some(mods) = mods {
                    // Unmount by id, whether or not this resource saw it mounted (a mount the game
                    // made itself, or one a failed update left behind).
                    if mods.unmount(&workshop_mod_id(*item)).is_ok() {
                        tracing::info!(target: "regia_steam", item = item.0, "Workshop item unmounted from mods://");
                    }
                    status.mounted = None;
                }
            }
            WorkshopEvent::Installed { item, folder } => {
                let auto_mount = self.auto_mount;
                let status = self.items.entry(*item).or_default();
                status.subscribed = true;
                status.folder = Some(folder.clone());
                status.installs += 1;
                if let (true, Some(mods)) = (auto_mount, mods) {
                    match mount_item(mods, *item, folder) {
                        Ok(id) => {
                            status.mounted = Some(id);
                            status.error = None;
                        }
                        Err(e) => {
                            tracing::warn!(target: "regia_steam", item = item.0, "Workshop item not mounted: {e}");
                            status.mounted = None;
                            status.error = Some(e.to_string());
                        }
                    }
                }
            }
        }
    }
}

/// The item id of a `workshop.<id>` mod.
fn workshop_item_of(mod_id: &str) -> Option<u64> {
    mod_id.strip_prefix("workshop.")?.parse().ok()
}

/// Mounts an installed item. An update remounts in the same load-order place. A new item goes
/// before the first mounted Workshop item with a larger id (else at the end), so the same
/// subscriptions give the same order, and the same content-set hash, whatever order Steam installed
/// them in. On any failure the item is left unmounted: an old mount of it is not kept, because its
/// files changed under it.
fn mount_item(mods: &mut ModsVfs, item: WorkshopItemId, folder: &Path) -> Result<String, ModsError> {
    let id = workshop_mod_id(item);
    let order = mods.order();
    let place = order.iter().position(|m| *m == id);
    if place.is_some() {
        mods.unmount(&id)?;
    }
    let source = ModSource::detect(folder)?;
    mods.mount(&id, source)?;
    let mut wanted = order;
    wanted.retain(|m| *m != id);
    let at = place.unwrap_or_else(|| {
        wanted
            .iter()
            .position(|m| workshop_item_of(m).is_some_and(|other| other > item.0))
            .unwrap_or(wanted.len())
    });
    wanted.insert(at.min(wanted.len()), id.clone());
    if let Err(e) = mods.set_order(&wanted) {
        let _ = mods.unmount(&id);
        return Err(e);
    }
    Ok(id)
}

/// The lobby key [`MODS_LOBBY_KEY`](regia_asset::mods_vfs::MODS_LOBBY_KEY) the plugin last published, per lobby.
#[derive(Resource, Debug, Default)]
struct PublishedModsKey {
    /// Lobby id and [`ModsVfs::revision`] last published. The hash is not recomputed while both match.
    last: Option<(regia_net::lobby::LobbyId, u64)>,
}

/// Wires the Steam Workshop: the [`SteamWorkshop`] resource (and [`ModsVfs`] when missing), the
/// per-frame poll that mounts installed items, the lobby key sync and the `steam.workshop_*` agent
/// methods. Add it after [`crate::SteamPlugin`]. Without an injected service it uses the real
/// Workshop once Steam is ready (with the `steamworks` feature), else none.
#[derive(Clone, Default)]
pub struct SteamWorkshopPlugin {
    service: Option<Arc<dyn WorkshopService>>,
    manual_mount: bool,
    agent_upload: bool,
    no_lobby_sync: bool,
}

impl std::fmt::Debug for SteamWorkshopPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SteamWorkshopPlugin")
            .field("service", &self.service.as_ref().map(|s| s.name()))
            .field("manual_mount", &self.manual_mount)
            .finish_non_exhaustive()
    }
}

impl SteamWorkshopPlugin {
    /// Uses `service` (a [`MockWorkshop`], or a host's own) instead of the real Workshop.
    #[must_use]
    pub fn with_service(service: Arc<dyn WorkshopService>) -> Self {
        Self {
            service: Some(service),
            ..Self::default()
        }
    }

    /// Leaves mounting to the game: installed items are reported but not mounted.
    #[must_use]
    pub fn manual_mount(mut self) -> Self {
        self.manual_mount = true;
        self
    }

    /// Lets the agent (`steam.workshop_upload`) upload to the real Workshop. Off by default:
    /// an upload publishes outside the machine. The mock always allows it.
    #[must_use]
    pub fn allow_agent_upload(mut self) -> Self {
        self.agent_upload = true;
        self
    }

    /// Leaves the lobby key [`MODS_LOBBY_KEY`](regia_asset::mods_vfs::MODS_LOBBY_KEY) to the game.
    #[must_use]
    pub fn without_lobby_sync(mut self) -> Self {
        self.no_lobby_sync = true;
        self
    }
}

#[cfg(all(feature = "steamworks", not(target_os = "android")))]
fn real_service(steam: &Steam) -> Option<Arc<dyn WorkshopService>> {
    steam
        .client()
        .map(|client| Arc::new(RealWorkshop::new(client)) as Arc<dyn WorkshopService>)
}

#[cfg(not(all(feature = "steamworks", not(target_os = "android"))))]
fn real_service(_steam: &Steam) -> Option<Arc<dyn WorkshopService>> {
    None
}

/// Resources `workshop_system` borrows together. `SystemState` keeps them in place.
/// `World::resource_scope` would remove and reinsert `SteamWorkshop` and mark it changed
/// on every poll.
type WorkshopPoll = (
    ResMut<'static, SteamWorkshop>,
    Option<ResMut<'static, ModsVfs>>,
    Option<Res<'static, Steam>>,
);

fn workshop_system(world: &mut World, state: &mut SystemState<WorkshopPoll>) {
    if world.get_resource::<SteamWorkshop>().is_none() {
        return;
    }
    {
        let Ok((mut workshop, mut mods, steam)) = state.get_mut(world) else {
            return;
        };
        let dirty = {
            let workshop = workshop.bypass_change_detection();
            drive_workshop(workshop, steam.as_deref(), mods.as_mut())
        };
        if dirty {
            workshop.set_changed();
        }
    }
    // `get_mut` queues nothing here. `apply` drops that queue before the next frame.
    state.apply(world);
}

/// Polls `workshop` in place. Returns whether a caller should see the resource as changed:
/// the service was attached or detached, or the poll applied at least one event.
fn drive_workshop(
    workshop: &mut SteamWorkshop,
    steam: Option<&Steam>,
    mods: Option<&mut ResMut<ModsVfs>>,
) -> bool {
    let mut dirty = false;
    if !workshop.injected {
        let ready = steam.is_some_and(|s| s.state() == SteamState::Ready);
        let attached = workshop.service.name() != "unavailable";
        if ready && !attached {
            if let Some(service) = steam.and_then(real_service) {
                workshop.service = service;
                workshop.adopted = false;
                dirty = true;
                tracing::info!(target: "regia_steam", "Steam Workshop attached");
            }
        } else if !ready && attached {
            workshop.service = Arc::new(UnavailableWorkshop);
            dirty = true;
            tracing::info!(target: "regia_steam", "Steam Workshop detached (Steam is gone)");
        }
    }
    let events = if let Some(mods) = mods {
        // Only a mount or an unmount marks the resource changed, not every frame's poll.
        let before = mods.revision();
        let events = workshop.update(Some(mods.bypass_change_detection()));
        if mods.revision() != before {
            mods.set_changed();
        }
        events
    } else {
        workshop.update(None)
    };
    dirty || !events.is_empty()
}

/// Keeps [`MODS_LOBBY_KEY`](regia_asset::mods_vfs::MODS_LOBBY_KEY) of the lobby this player owns equal to the mounted set's hash.
fn sync_lobby_key(
    mods: Option<Res<ModsVfs>>,
    lobbies: Option<ResMut<regia_net::lobby::Lobbies>>,
    mut published: ResMut<PublishedModsKey>,
) {
    let (Some(mods), Some(mut lobbies)) = (mods, lobbies) else {
        return;
    };
    let Some(lobby) = lobbies.current().filter(|_| lobbies.is_owner()) else {
        published.last = None;
        return;
    };
    let revision = mods.revision();
    if published.last == Some((lobby, revision)) {
        return;
    }
    let (key, value) = mods.lobby_entry();
    match lobbies.set_data(key, Some(&value)) {
        Ok(()) => published.last = Some((lobby, revision)),
        Err(e) => tracing::debug!(target: "regia_steam", "lobby key {key} not set: {e}"),
    }
}

impl Plugin for SteamWorkshopPlugin {
    fn build(&self, app: &mut App) {
        if app.world().get_resource::<ModsVfs>().is_none() {
            app.insert_resource(ModsVfs::new());
        }
        let mut workshop = match &self.service {
            Some(service) => SteamWorkshop::new(Arc::clone(service)),
            None => SteamWorkshop::unattached(),
        };
        workshop.auto_mount = !self.manual_mount;
        workshop.agent_upload = self.agent_upload;
        app.insert_resource(workshop);
        app.add_systems(PreUpdate, workshop_system.after(crate::pump::pump_steam));
        if !self.no_lobby_sync {
            app.init_resource::<PublishedModsKey>();
            app.add_systems(
                PreUpdate,
                sync_lobby_key
                    .after(workshop_system)
                    .before(regia_net::lobby::lobby_system),
            );
        }
        crate::workshop_agent::install(app.world_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_visibility_names_round_trip() {
        for v in [
            WorkshopVisibility::Public,
            WorkshopVisibility::FriendsOnly,
            WorkshopVisibility::Private,
            WorkshopVisibility::Unlisted,
        ] {
            assert_eq!(WorkshopVisibility::from_name(v.name()), Some(v));
        }
        let errors = [
            WorkshopError::Unavailable,
            WorkshopError::Invalid(String::new()),
            WorkshopError::UnknownItem(1),
            WorkshopError::Platform(String::new()),
            WorkshopError::UploadNotAllowed,
        ];
        for e in errors {
            assert!(WorkshopError::CODES.contains(&e.code()), "{e:?}");
        }
        assert_eq!(workshop_mod_id(WorkshopItemId(42)), "workshop.42");
        assert!(regia_asset::mods_vfs::is_valid_mod_id(&workshop_mod_id(
            WorkshopItemId(u64::MAX)
        )));
    }

    #[test]
    fn uploads_are_checked_before_anything_is_sent() {
        let mut upload = WorkshopUpload::new("", "/nonexistent-regia-dir");
        assert!(matches!(upload.check(), Err(WorkshopError::Invalid(_))));
        upload.title = "x".repeat(MAX_TITLE_BYTES + 1);
        assert!(matches!(upload.check(), Err(WorkshopError::Invalid(_))));
        upload.title = "ok".into();
        upload.tags = vec!["a,b".into()];
        assert!(matches!(upload.check(), Err(WorkshopError::Invalid(_))));
        upload.tags.clear();
        assert!(
            matches!(upload.check(), Err(WorkshopError::Invalid(_))),
            "missing folder"
        );
        let mut workshop = SteamWorkshop::new(Arc::new(UnavailableWorkshop));
        assert!(workshop.upload(&upload).is_err());
        assert!(workshop.uploads().is_empty());
    }

    #[test]
    fn an_update_with_no_installable_files_keeps_the_installed_tree() {
        // Paths that are not valid `mods://` paths are dropped. An update made only of those
        // paths must not replace a good install with an empty directory.
        let root = regia_test_utils::unique_temp_dir("steam-workshop-empty-update");
        let mock = MockWorkshop::shared(&root);
        let item = mock.publish("Pack", BTreeMap::from([("a.txt".to_owned(), b"v1".to_vec())]));
        WorkshopService::subscribe(mock.as_ref(), item).expect("subscribe");
        let _ = mock.poll();
        let folder = root.join(item.0.to_string());
        assert_eq!(std::fs::read(folder.join("a.txt")).expect("installed"), b"v1");

        assert!(mock.update_item(
            item,
            BTreeMap::from([("../escape.txt".to_owned(), b"no".to_vec())])
        ));
        let events = mock.poll();
        assert!(
            events.iter().any(|event| matches!(
                event,
                WorkshopEvent::SubscribeFailed { item: failed, .. } if *failed == item
            )),
            "the empty update must fail, got {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, WorkshopEvent::Installed { .. })),
            "the empty update must not be reported as installed, got {events:?}"
        );
        assert_eq!(
            std::fs::read(folder.join("a.txt")).expect("previous tree kept"),
            b"v1"
        );
        assert!(!root.join("escape.txt").exists(), "a rejected path was written");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_first_install_with_no_installable_files_creates_no_folder() {
        // The same refusal applies when nothing was installed yet: no item folder, no staging
        // directory, and no `Installed` event.
        let root = regia_test_utils::unique_temp_dir("steam-workshop-empty-first");
        let mock = MockWorkshop::shared(&root);
        let item = mock.publish(
            "Empty",
            BTreeMap::from([("../escape.txt".to_owned(), b"no".to_vec())]),
        );
        WorkshopService::subscribe(mock.as_ref(), item).expect("subscribe");
        let events = mock.poll();
        assert!(
            events.iter().any(|event| matches!(
                event,
                WorkshopEvent::SubscribeFailed { item: failed, .. } if *failed == item
            )),
            "the empty install must fail, got {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, WorkshopEvent::Installed { .. })),
            "the empty install must not be reported as installed, got {events:?}"
        );
        assert!(
            !root.join(item.0.to_string()).exists(),
            "empty install created a folder"
        );
        assert!(!root.join("escape.txt").exists(), "a rejected path was written");
        let leftovers: Vec<String> = std::fs::read_dir(&root)
            .expect("read root")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            leftovers,
            Vec::<String>::new(),
            "empty install left {leftovers:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unsubscribe_of_an_unsubscribed_item_is_unknown_and_a_later_failure_is_platform() {
        let item = WorkshopItemId(1);
        let err = unsubscribe_if_subscribed(item, false).expect_err("not subscribed");
        assert_eq!(err, WorkshopError::UnknownItem(1));
        assert_eq!(err.code(), "workshop_unknown_item");
        assert!(unsubscribe_if_subscribed(item, true).is_ok());
        let later = WorkshopError::Platform("not subscribed".into());
        assert_eq!(later.code(), "workshop_platform");
        assert!(later.to_string().starts_with("Steam:"), "{later}");
    }

    #[test]
    fn a_quiet_library_is_not_queried_between_scans_and_a_download_is_not_polled_every_frame() {
        assert!(poll_query(0).scan, "the adopting poll scans");
        let mut waiting_queries = 0u32;
        for index in 1..SCAN_EVERY_POLLS {
            let query = poll_query(index);
            assert!(
                !query.scan,
                "poll {index} is inside the 119 quiet polls after the scan"
            );
            if query.query_waiting {
                waiting_queries += 1;
            }
        }
        assert!(
            waiting_queries < SCAN_EVERY_POLLS as u32 - 1,
            "a waiting item was queried on {waiting_queries} of the 119 polls"
        );
        let stable = WorkshopItemState {
            subscribed: true,
            installed: true,
            downloading: false,
            needs_update: false,
        };
        assert!(
            !should_read_install_info(true, true, stable),
            "a known item whose state bits did not change must not call install_info"
        );
        assert!(
            should_read_install_info(false, true, stable),
            "the first sight of an install is read"
        );
        let updating = WorkshopItemState {
            needs_update: true,
            ..stable
        };
        assert!(!should_read_install_info(true, false, updating));
    }

    /// An installed item that Steam updates (stable, then needs-update, downloading, stable again)
    /// must be read again when it settles, so the new install is mounted and reported. The poll
    /// recorded state bits only on a stable read, so the settled state compared equal to the
    /// pre-update bits and the update was never picked up.
    #[test]
    fn an_update_seen_downloading_is_read_when_it_settles() {
        let pack = pack_state;
        let stable = WorkshopItemState {
            subscribed: true,
            installed: true,
            downloading: false,
            needs_update: false,
        };
        let needs_update = WorkshopItemState {
            needs_update: true,
            ..stable
        };
        let downloading = WorkshopItemState {
            downloading: true,
            ..needs_update
        };
        let mut known: BTreeMap<u64, (u32, u32)> = BTreeMap::new();
        // The first stable read stores the timestamp and the bits.
        known.insert(7, (100, pack(stable)));
        for state in [needs_update, downloading] {
            note_unstable_state(&mut known, 7, pack(state));
        }
        let unchanged = known.get(&7).is_some_and(|(_, bits)| *bits == pack(stable));
        assert!(
            should_read_install_info(true, unchanged, stable),
            "the settled update must be read again"
        );
        // An id never read stable is not invented.
        note_unstable_state(&mut known, 8, pack(downloading));
        assert!(!known.contains_key(&8));
    }

    /// An update that starts and finishes between two scans never shows its downloading bits; the
    /// periodic recheck re-reads every known install's timestamp so it is still picked up.
    #[test]
    fn known_installs_are_rechecked_periodically_but_not_every_scan() {
        let period = SCAN_EVERY_POLLS * RECHECK_EVERY_SCANS;
        assert!(poll_query(0).recheck_installed);
        assert!(poll_query(period).recheck_installed && poll_query(period).scan);
        assert!(!poll_query(SCAN_EVERY_POLLS).recheck_installed);
        let rechecks = (0..period * 2)
            .filter(|&i| poll_query(i).recheck_installed)
            .count();
        assert_eq!(rechecks, 2);
    }

    #[test]
    fn replace_keeps_the_previous_folder_when_the_swap_fails() {
        let root = regia_test_utils::unique_temp_dir("workshop-replace-fails");
        let folder = root.join("item");
        let staging = root.join("staging");
        let aside = root.join("aside");
        std::fs::create_dir_all(folder.join("sub")).expect("old install");
        std::fs::write(folder.join("sub/old.txt"), b"old").expect("old file");

        // The staged tree is gone before the swap (a failed download cleaned it, say): the
        // second rename fails and the previous install must come back from `aside`.
        let err = replace_tree(&staging, &folder, &aside).expect_err("swap fails");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(
            std::fs::read(folder.join("sub/old.txt")).expect("old file readable"),
            b"old",
            "the previous install survived the failed replace"
        );
        assert!(!aside.exists(), "the aside copy was moved back");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn replace_swaps_in_the_new_tree_and_drops_the_old() {
        let root = regia_test_utils::unique_temp_dir("workshop-replace-ok");
        let folder = root.join("item");
        let staging = root.join("staging");
        let aside = root.join("aside");
        std::fs::create_dir_all(&folder).expect("old install");
        std::fs::write(folder.join("old.txt"), b"old").expect("old file");
        std::fs::create_dir_all(&staging).expect("staging");
        std::fs::write(staging.join("new.txt"), b"new").expect("new file");

        replace_tree(&staging, &folder, &aside).expect("swap succeeds");
        assert_eq!(std::fs::read(folder.join("new.txt")).expect("new file"), b"new");
        assert!(!folder.join("old.txt").exists(), "the old tree is gone");
        assert!(!aside.exists(), "the old copy was dropped");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The files of `dir` with their bytes, relative paths with `/`.
    fn tree_bytes(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        tree_files(dir)
            .expect("list")
            .into_iter()
            .map(|rel| {
                let bytes = std::fs::read(dir.join(&rel)).expect("read");
                (rel.to_string_lossy().replace('\\', "/"), bytes)
            })
            .collect()
    }

    fn write_tree(dir: &Path, files: &[(&str, &str)]) {
        for (rel, text) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, text).expect("write");
        }
    }

    /// ADR-8504: the file-by-file swap (the path Windows takes while a file of the install is
    /// open) leaves exactly the new tree in the same folder, nested folders included, and drops
    /// the old files and the emptied staging folder.
    #[test]
    fn replace_entries_swaps_file_by_file_into_the_same_folder() {
        let root = regia_test_utils::unique_temp_dir("workshop-replace-entries");
        let (folder, staging, aside) = (root.join("item"), root.join("staging"), root.join("aside"));
        write_tree(
            &folder,
            &[("a.txt", "v1"), ("sub/old.txt", "old"), ("gone/deep/x.txt", "x")],
        );
        write_tree(
            &staging,
            &[("a.txt", "v2"), ("sub/new.txt", "new"), ("b.txt", "b")],
        );
        // A reader holds an old file open across the swap (fine on every OS for a file).
        let held = std::fs::File::open(folder.join("a.txt")).expect("open");
        replace_entries(&staging, &folder, &aside).expect("swap");
        drop(held);
        let expected: BTreeMap<String, Vec<u8>> = [("a.txt", "v2"), ("b.txt", "b"), ("sub/new.txt", "new")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.as_bytes().to_vec()))
            .collect();
        assert_eq!(tree_bytes(&folder), expected);
        assert!(
            !folder.join("gone").exists(),
            "the old tree's emptied folders are gone"
        );
        assert!(!staging.exists(), "the emptied staging folder is removed");
        assert!(!aside.exists(), "the old files are dropped");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADR-8504: a file of the old install that cannot move aside stops the swap, the files that
    /// already moved come back, and the staged tree is left for the caller to remove.
    #[test]
    fn replace_entries_puts_the_old_files_back_when_one_cannot_move() {
        let root = regia_test_utils::unique_temp_dir("workshop-replace-entries-undo");
        let (folder, staging, aside) = (root.join("item"), root.join("staging"), root.join("aside"));
        write_tree(&folder, &[("a.txt", "v1"), ("sub/old.txt", "old")]);
        write_tree(&staging, &[("a.txt", "v2")]);
        // A file named `sub` in `aside` blocks the folder `aside/sub`: `a.txt` moves first (sorted
        // order), then `sub/old.txt` cannot.
        write_tree(&aside, &[("sub", "blocker")]);
        replace_entries(&staging, &folder, &aside).expect_err("the second move fails");
        let expected: BTreeMap<String, Vec<u8>> = [("a.txt", "v1"), ("sub/old.txt", "old")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.as_bytes().to_vec()))
            .collect();
        assert_eq!(
            tree_bytes(&folder),
            expected,
            "the previous install is whole again"
        );
        assert_eq!(
            std::fs::read(staging.join("a.txt")).expect("staged file"),
            b"v2",
            "the staged tree is untouched"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn download_failures_are_reported_once_per_streak() {
        let failures = Mutex::new(BTreeMap::new());
        assert!(
            record_download_failure(&failures, 7, 4),
            "the first failure of a streak surfaces"
        );
        for _ in 1..3 {
            assert!(
                !record_download_failure(&failures, 7, 4),
                "the rest of the streak is quiet"
            );
        }
        assert!(
            record_download_failure(&failures, 7, 4),
            "a reminder on every 4th failure"
        );
        // A different item has its own streak.
        assert!(record_download_failure(&failures, 8, 4));
    }

    /// corr-low: a `download_item` that is accepted clears the streak, so a later refusal starts
    /// a new streak (its first failure surfaces) instead of continuing the old count and only
    /// reminding every Nth failure from it. `clear_download_failures` did not exist before this
    /// fix, so this is the regression test for the missing reset.
    #[test]
    fn a_successful_download_resets_the_failure_streak() {
        let failures = Mutex::new(BTreeMap::new());
        assert!(record_download_failure(&failures, 7, 4));
        for _ in 1..3 {
            assert!(!record_download_failure(&failures, 7, 4));
        }
        clear_download_failures(&failures, 7);
        assert!(
            record_download_failure(&failures, 7, 4),
            "the first failure of the new streak surfaces again"
        );
    }

    /// A service that replays scripted poll events, for `SteamWorkshop::update` tests.
    struct ScriptedService(Vec<WorkshopEvent>);

    impl WorkshopService for ScriptedService {
        fn name(&self) -> &'static str {
            "mock"
        }
        fn upload(
            &self,
            _upload: &WorkshopUpload,
            _metadata: &str,
        ) -> Result<WorkshopRequest, WorkshopError> {
            Err(WorkshopError::Unavailable)
        }
        fn subscribe(&self, _item: WorkshopItemId) -> Result<(), WorkshopError> {
            Err(WorkshopError::Unavailable)
        }
        fn unsubscribe(&self, _item: WorkshopItemId) -> Result<(), WorkshopError> {
            Err(WorkshopError::Unavailable)
        }
        fn subscribed(&self) -> Vec<WorkshopItemId> {
            Vec::new()
        }
        fn item_state(&self, _item: WorkshopItemId) -> WorkshopItemState {
            WorkshopItemState::default()
        }
        fn install_info(&self, _item: WorkshopItemId) -> Option<InstalledItem> {
            None
        }
        fn poll(&self) -> Vec<WorkshopEvent> {
            self.0.clone()
        }
    }

    #[test]
    fn apply_records_download_and_unsubscribe_failures_on_the_item() {
        let item = WorkshopItemId(1);
        let mut workshop = SteamWorkshop::new(Arc::new(ScriptedService(vec![
            WorkshopEvent::Subscribed { item },
            WorkshopEvent::DownloadFailed {
                item,
                error: WorkshopError::Platform("could not start".into()),
            },
            WorkshopEvent::UnsubscribeFailed {
                item,
                error: WorkshopError::Platform("refused".into()),
            },
        ])));
        let events = workshop.update(None);
        assert_eq!(events.len(), 3, "every event is delivered once");
        let status = &workshop.items()[&item];
        assert!(
            status.subscribed,
            "an unsubscribe failure leaves the item subscribed"
        );
        assert!(
            status
                .error
                .as_deref()
                .is_some_and(|e| e.contains("unsubscribe failed")),
            "the failure says the unsubscribe failed, not the subscribe: {status:?}"
        );
    }
}
