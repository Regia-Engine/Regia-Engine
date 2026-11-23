// SPDX-License-Identifier: MPL-2.0
//! `mods://`: the read-only, confined file system of a game's mods (spec 84 § 13, spec 24 § 2,
//! ADR-2418).
//!
//! A mod is a folder, a `.fpak` archive, or an in-memory file map ([`ModSource`]). [`ModsVfs`]
//! mounts mods in a **declared load order**: the first mod is the base, each later one is laid over
//! it, and where two mods hold the same path the later one wins. Every override is reported
//! ([`ModsVfs::conflicts`]). The mount is **read-only** ([`ModsVfs::write`] is always refused) and
//! **confined**: a path is a relative `mods://` path, never absolute and never with `..`; a mod
//! folder may not hold a symbolic link (or, on Windows, any reparse point such as a junction)
//! anywhere below its root, and a read walks the path again so a link swapped in after the mount
//! is refused too. Only the files indexed when the mod mounted can be read at all.
//!
//! **Content hashes.** Each mod has a content hash: BLAKE3 (derived key
//! `regia mods file listing v1`) over its files sorted by path, each as the path's byte length
//! (`u64` LE), the path, the file size (`u64` LE) and the file's own BLAKE3 hash. The mod id and the
//! source kind are not part of it, so the same files as a folder, a pak or a Workshop download
//! hash the same. [`ModsVfs::content_set_hash`] combines the mounted set (derived key
//! `regia mods content set v1`, the number of mods as `u64` LE, then each mod's content hash in
//! load order), so it is stable across runs and machines and changes with the order. A file that
//! changed after its mod mounted is refused on read (`mods_changed`): what a game reads is what
//! the hash says. Games put the hash in their lobby data under [`MODS_LOBBY_KEY`]
//! ([`ModsVfs::lobby_entry`]) and match lobbies on it.
//!
//! Error codes ([`ModsErrorCode::code`]) are part of the agent protocol (`book/agents/mods.md`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_app::prelude::*;
use regia_platform::{MountPoint, VfsDirEntry, VfsPath};

use crate::{AssetId, PakReader};

/// The scheme of the mods file system (`mods://textures/rock.ktx2`).
pub const MODS_SCHEME: &str = "mods";

/// The lobby data key a game publishes [`ModsVfs::content_set_hash`] under (64 lowercase hex
/// digits). Not under the engine's reserved `regia.` prefix: a game sets it like any other key.
pub const MODS_LOBBY_KEY: &str = "mods.content_set";

/// Longest mod id in bytes.
pub const MAX_MOD_ID_BYTES: usize = 64;

/// Longest relative path in bytes.
pub const MAX_PATH_BYTES: usize = 1024;

/// Longest mod version text in bytes ([`ModInfo::version`]; the session manifest carries 64).
pub const MAX_MOD_VERSION_BYTES: usize = 64;

/// The file at a mod's root whose `version` field is the mod's version text (spec 84 § 3).
pub const MOD_VERSION_FILE: &str = "luamod.ron";

const FILE_LISTING_CONTEXT: &str = "regia mods file listing v1";
const CONTENT_SET_CONTEXT: &str = "regia mods content set v1";

/// Why a `mods://` operation failed. The [`code`](Self::code) strings are part of the agent protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModsErrorCode {
    /// The path is not a valid `mods://` path (another scheme, a backslash, a control character,
    /// a reserved device name, an empty segment).
    PathInvalid,
    /// The path tries to leave the mount: absolute, a drive letter, or a `..` segment.
    PathEscape,
    /// A symbolic link or reparse point below a mod's root.
    Symlink,
    /// No mounted mod holds that file or directory.
    NotFound,
    /// No mod with that id is mounted.
    UnknownMod,
    /// A mod with that id is already mounted.
    Duplicate,
    /// A mod id is not `[a-z0-9_.-]`, 1..=64 bytes.
    IdInvalid,
    /// Reading a mod's folder failed.
    Io,
    /// A mod's pak could not be opened or read.
    Pak,
    /// A mod holds more files or bytes than [`ModsLimits`] allow, or too many mods are mounted.
    TooLarge,
    /// Writing: `mods://` is read-only.
    ReadOnly,
    /// A load order that is not exactly the mounted mods, each once.
    OrderInvalid,
    /// A file changed after its mod mounted (size or content hash differs).
    Changed,
}

impl ModsErrorCode {
    /// Every code, in protocol order.
    pub const ALL: [ModsErrorCode; 13] = [
        ModsErrorCode::PathInvalid,
        ModsErrorCode::PathEscape,
        ModsErrorCode::Symlink,
        ModsErrorCode::NotFound,
        ModsErrorCode::UnknownMod,
        ModsErrorCode::Duplicate,
        ModsErrorCode::IdInvalid,
        ModsErrorCode::Io,
        ModsErrorCode::Pak,
        ModsErrorCode::TooLarge,
        ModsErrorCode::ReadOnly,
        ModsErrorCode::OrderInvalid,
        ModsErrorCode::Changed,
    ];

    /// The protocol string (`mods_path_escape`, …).
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            ModsErrorCode::PathInvalid => "mods_path_invalid",
            ModsErrorCode::PathEscape => "mods_path_escape",
            ModsErrorCode::Symlink => "mods_symlink",
            ModsErrorCode::NotFound => "mods_not_found",
            ModsErrorCode::UnknownMod => "mods_unknown_mod",
            ModsErrorCode::Duplicate => "mods_duplicate",
            ModsErrorCode::IdInvalid => "mods_id_invalid",
            ModsErrorCode::Io => "mods_io",
            ModsErrorCode::Pak => "mods_pak",
            ModsErrorCode::TooLarge => "mods_too_large",
            ModsErrorCode::ReadOnly => "mods_read_only",
            ModsErrorCode::OrderInvalid => "mods_order_invalid",
            ModsErrorCode::Changed => "mods_changed",
        }
    }
}

impl std::fmt::Display for ModsErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

/// A failed `mods://` operation: the protocol code and a message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ModsError {
    /// The protocol code.
    pub code: ModsErrorCode,
    /// What happened, naming the path or mod.
    pub message: String,
}

impl ModsError {
    fn new(code: ModsErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Size limits of the mounted set. Defaults: 256 mods, 65,536 files and 8 GiB per mod, 1 GiB per
/// file and per decompressed pak entry, folders 32 deep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModsLimits {
    /// Most mods mounted at once.
    pub max_mods: usize,
    /// Most files in one mod.
    pub max_files_per_mod: usize,
    /// Most bytes of all of one mod's files.
    pub max_mod_bytes: u64,
    /// Most bytes of one file. A read returns a file whole, and a pak entry is decompressed whole
    /// at mount to hash it, so this bounds one allocation a mod can ask for (ADR-5870). For a pak
    /// mod it is also the most one entry may decompress to (lz4 at the reader's 64:1 cap turns a
    /// 16 MiB entry into 1 GiB), checked from the table of contents before any entry is read
    /// (ADR-5869; one field for both caps since ADR-5993).
    pub max_file_bytes: u64,
    /// Deepest folder nesting below a mod's root.
    pub max_depth: usize,
}

impl Default for ModsLimits {
    fn default() -> Self {
        Self {
            max_mods: 256,
            max_files_per_mod: 65_536,
            max_mod_bytes: 8 * 1024 * 1024 * 1024,
            max_file_bytes: 1024 * 1024 * 1024,
            max_depth: 32,
        }
    }
}

/// Where a mod's files come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModSource {
    /// A folder on the host file system. The root itself may be a symbolic link (a Workshop or
    /// launcher folder often is); nothing below it may be.
    Dir(PathBuf),
    /// A `.fpak` archive; its files are its manifest paths.
    Pak(PathBuf),
    /// Files in memory: relative path → bytes (tests, packed mods, Android reads).
    Memory(BTreeMap<String, Vec<u8>>),
}

impl ModSource {
    /// The source for a path: a `.fpak` file is a pak, a folder holding nothing but one `.fpak`
    /// file is that pak (a Workshop item of a pak), any other folder is a folder.
    pub fn detect(path: &Path) -> Result<ModSource, ModsError> {
        let meta = fs::metadata(path)
            .map_err(|e| ModsError::new(ModsErrorCode::Io, format!("{}: {e}", path.display())))?;
        if meta.is_file() {
            return Ok(ModSource::Pak(path.to_path_buf()));
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(path)
            .map_err(|e| ModsError::new(ModsErrorCode::Io, format!("{}: {e}", path.display())))?
        {
            let entry =
                entry.map_err(|e| ModsError::new(ModsErrorCode::Io, format!("{}: {e}", path.display())))?;
            entries.push(entry.path());
            if entries.len() > 1 {
                return Ok(ModSource::Dir(path.to_path_buf()));
            }
        }
        if let [only] = entries.as_slice() {
            let is_pak = only.extension().is_some_and(|e| e.eq_ignore_ascii_case("fpak"));
            let plain_file = fs::symlink_metadata(only).is_ok_and(|m| m.is_file() && !is_link(&m));
            if is_pak && plain_file {
                return Ok(ModSource::Pak(only.clone()));
            }
        }
        Ok(ModSource::Dir(path.to_path_buf()))
    }

    /// `dir`, `pak` or `memory`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            ModSource::Dir(_) => "dir",
            ModSource::Pak(_) => "pak",
            ModSource::Memory(_) => "memory",
        }
    }
}

/// The relative path of a `mods://` path (`mods://a/b.lua` or plain `a/b.lua`): `/`-separated,
/// `.` segments dropped, empty for the root. Absolute paths, drive letters and `..` are refused with
/// [`ModsErrorCode::PathEscape`]; other schemes and everything a VFS path refuses (backslashes,
/// control characters, `:`, reserved device names) with [`ModsErrorCode::PathInvalid`].
pub fn mods_relative(path: &str) -> Result<String, ModsError> {
    let relative = match path.split_once("://") {
        Some((MODS_SCHEME, rest)) => rest,
        Some((other, _)) => {
            return Err(ModsError::new(
                ModsErrorCode::PathInvalid,
                format!("`{path}`: scheme `{other}://` is not mods://"),
            ));
        }
        None => path,
    };
    if relative.len() > MAX_PATH_BYTES {
        return Err(ModsError::new(
            ModsErrorCode::PathInvalid,
            format!("path is {} bytes; at most {MAX_PATH_BYTES}", relative.len()),
        ));
    }
    let escape = |why: &str| ModsError::new(ModsErrorCode::PathEscape, format!("`{path}`: {why}"));
    if relative.starts_with('/') || relative.starts_with('\\') {
        return Err(escape("absolute paths leave the mount"));
    }
    let bytes = relative.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(escape("a drive letter leaves the mount"));
    }
    if relative.split(['/', '\\']).any(|segment| segment == "..") {
        return Err(escape("`..` leaves the mount"));
    }
    let checked = VfsPath::new(MountPoint::Assets, relative)
        .map_err(|e| ModsError::new(ModsErrorCode::PathInvalid, format!("`{path}`: {e}")))?;
    Ok(checked.relative().to_owned())
}

/// Whether `id` is a valid mod id: `[a-z0-9_.-]`, 1..=64 bytes (the `luamod.ron` id rule).
#[must_use]
pub fn is_valid_mod_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_MOD_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-'))
}

/// A 256-bit content hash (a mod's or the mounted set's).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct ContentSetHash(pub [u8; 32]);

impl ContentSetHash {
    /// 64 lowercase hex digits.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }

    /// The first 8 bytes as a little-endian `u64` (for a field that holds a 64-bit hash).
    #[must_use]
    pub fn short(&self) -> u64 {
        let mut first = [0u8; 8];
        first.copy_from_slice(&self.0[..8]);
        u64::from_le_bytes(first)
    }

    /// Parses 64 hex digits (either case).
    #[must_use]
    pub fn from_hex(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            out[i] = u8::try_from(hi * 16 + lo).ok()?;
        }
        Some(Self(out))
    }
}

impl std::fmt::Display for ContentSetHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// A mod's content hash from its files: `(relative path, size, BLAKE3 of the bytes)`, in any order
/// (they are sorted by path here). The module docs give the exact byte layout.
#[must_use]
pub fn mod_content_hash<'a>(files: impl IntoIterator<Item = (&'a str, u64, &'a [u8; 32])>) -> ContentSetHash {
    let mut sorted: Vec<_> = files.into_iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut hasher = blake3::Hasher::new_derive_key(FILE_LISTING_CONTEXT);
    for (path, size, hash) in sorted {
        hasher.update(&(path.len() as u64).to_le_bytes());
        hasher.update(path.as_bytes());
        hasher.update(&size.to_le_bytes());
        hasher.update(hash);
    }
    ContentSetHash(*hasher.finalize().as_bytes())
}

/// The content-set hash of mods with these content hashes, in load order (first = base). This is
/// what [`ModsVfs::content_set_hash`] returns for its mounted set.
#[must_use]
pub fn content_set_hash<'a>(in_load_order: impl IntoIterator<Item = &'a ContentSetHash>) -> ContentSetHash {
    let hashes: Vec<&ContentSetHash> = in_load_order.into_iter().collect();
    let mut hasher = blake3::Hasher::new_derive_key(CONTENT_SET_CONTEXT);
    hasher.update(&(hashes.len() as u64).to_le_bytes());
    for hash in hashes {
        hasher.update(&hash.0);
    }
    ContentSetHash(*hasher.finalize().as_bytes())
}

/// What one indexed file was when its mod mounted.
#[derive(Debug, Clone, Copy)]
struct FileRecord {
    size: u64,
    hash: [u8; 32],
    /// The file's (device, inode) on Unix when it came from a folder: a read must open the same
    /// file, not one swapped in under its name.
    identity: Option<(u64, u64)>,
}

#[derive(Clone)]
enum Store {
    Dir(PathBuf),
    Pak {
        reader: Arc<PakReader>,
        ids: Arc<BTreeMap<String, AssetId>>,
    },
    Memory(Arc<BTreeMap<String, Vec<u8>>>),
}

#[derive(Clone)]
struct MountedMod {
    id: String,
    kind: &'static str,
    origin: String,
    store: Store,
    files: Arc<BTreeMap<String, FileRecord>>,
    bytes: u64,
    content_hash: ContentSetHash,
    /// Version text: the mod's own `luamod.ron` `version`, or what the mod manager set.
    version: String,
}

/// One mounted mod as [`ModsVfs::mods`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModInfo {
    /// The mod id.
    pub id: String,
    /// Place in the load order, 0 = base (lowest priority).
    pub order: usize,
    /// `dir`, `pak` or `memory`.
    pub kind: &'static str,
    /// The folder or pak path (empty for memory).
    pub origin: String,
    /// Number of files.
    pub files: usize,
    /// Bytes of all its files.
    pub bytes: u64,
    /// Files of this mod that a later mod overrides.
    pub overridden: usize,
    /// Its content hash.
    pub content_hash: ContentSetHash,
    /// Its version text (ADR-7259): the `version` of the `luamod.ron` at the mod's root when it
    /// mounted, or what [`ModsVfs::set_mod_version`] set; empty when the mod declares none. At most
    /// [`MAX_MOD_VERSION_BYTES`], without control characters.
    pub version: String,
}

/// A path more than one mod holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModConflict {
    /// The relative path.
    pub path: String,
    /// The mod whose file `mods://` serves (the latest in the load order).
    pub winner: String,
    /// The mods whose file it hides, in load order.
    pub overridden: Vec<String>,
}

/// The `mods://` file system (resource). Mods mount in load order; reads are served from the
/// latest mod holding a path. Cloning is cheap (stores are shared).
#[derive(Resource, Clone, Default)]
pub struct ModsVfs {
    limits: ModsLimits,
    mods: Vec<MountedMod>,
    /// Path → index into `mods` of the winner.
    index: BTreeMap<String, usize>,
    conflicts: Vec<ModConflict>,
    revision: u64,
}

impl std::fmt::Debug for ModsVfs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModsVfs")
            .field("order", &self.order())
            .field("files", &self.index.len())
            .field("conflicts", &self.conflicts.len())
            .finish()
    }
}

impl ModsVfs {
    /// An empty mount with the default limits.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty mount with `limits`.
    #[must_use]
    pub fn with_limits(limits: ModsLimits) -> Self {
        Self {
            limits,
            ..Self::default()
        }
    }

    /// The limits.
    #[must_use]
    pub fn limits(&self) -> ModsLimits {
        self.limits
    }

    /// Mounts mod `id` from `source` at the end of the load order (it overrides every mod mounted
    /// before it). The mod's files are walked and hashed now; nothing is mounted on an error.
    pub fn mount(&mut self, id: &str, source: ModSource) -> Result<ModInfo, ModsError> {
        let _span = tracing::info_span!("mods_vfs.mount", id, kind = source.kind()).entered();
        if !is_valid_mod_id(id) {
            return Err(ModsError::new(
                ModsErrorCode::IdInvalid,
                format!("`{id}`: mod ids are [a-z0-9_.-], 1..={MAX_MOD_ID_BYTES} bytes"),
            ));
        }
        if self.mods.iter().any(|m| m.id == id) {
            return Err(ModsError::new(
                ModsErrorCode::Duplicate,
                format!("`{id}` is already mounted"),
            ));
        }
        if self.mods.len() >= self.limits.max_mods {
            return Err(ModsError::new(
                ModsErrorCode::TooLarge,
                format!("at most {} mods may be mounted", self.limits.max_mods),
            ));
        }
        let mut mounted = build_mod(id, source, &self.limits)?;
        mounted.version = declared_version(&mounted);
        self.mods.push(mounted);
        self.rebuild();
        let conflicts = self.conflicts.len();
        tracing::info!(target: "regia_asset", id, order = self.mods.len() - 1, conflicts, "mods://: mounted");
        self.get(id)
            .ok_or_else(|| ModsError::new(ModsErrorCode::UnknownMod, format!("`{id}` vanished")))
    }

    /// Mounts several mods in this order (the declared load order), stopping at the first error.
    /// Mods mounted before the error stay mounted.
    pub fn mount_all<I, S>(&mut self, mods: I) -> Result<Vec<ModInfo>, ModsError>
    where
        I: IntoIterator<Item = (S, ModSource)>,
        S: AsRef<str>,
    {
        mods.into_iter()
            .map(|(id, source)| self.mount(id.as_ref(), source))
            .collect()
    }

    /// Sets mod `id`'s version text (ADR-7259): a mod manager that knows a version from elsewhere
    /// (a Workshop item, a store listing) overrides what the mod's `luamod.ron` declared. Trimmed,
    /// control characters dropped, cut to [`MAX_MOD_VERSION_BYTES`]. Bumps [`Self::revision`] when
    /// the text changes, so a session picks it up at its next start.
    pub fn set_mod_version(&mut self, id: &str, version: &str) -> Result<(), ModsError> {
        let at = self.position(id)?;
        let version = clean_version(version);
        if self.mods[at].version != version {
            self.mods[at].version = version;
            self.revision = self.revision.wrapping_add(1);
        }
        Ok(())
    }

    /// Unmounts mod `id`.
    pub fn unmount(&mut self, id: &str) -> Result<(), ModsError> {
        let at = self.position(id)?;
        self.mods.remove(at);
        self.rebuild();
        Ok(())
    }

    /// Unmounts every mod.
    pub fn clear(&mut self) {
        self.mods.clear();
        self.rebuild();
    }

    /// Sets the load order: `ids` must be exactly the mounted mods, each once; the first is the base.
    pub fn set_order<S: AsRef<str>>(&mut self, ids: &[S]) -> Result<(), ModsError> {
        let wanted: Vec<&str> = ids.iter().map(AsRef::as_ref).collect();
        let unique: BTreeSet<&str> = wanted.iter().copied().collect();
        let mounted: BTreeSet<&str> = self.mods.iter().map(|m| m.id.as_str()).collect();
        if unique.len() != wanted.len() || unique != mounted {
            let missing: Vec<&str> = mounted.difference(&unique).copied().collect();
            let unknown: Vec<&str> = unique.difference(&mounted).copied().collect();
            return Err(ModsError::new(
                ModsErrorCode::OrderInvalid,
                format!(
                    "the order must name every mounted mod once (missing {missing:?}, not mounted {unknown:?}, {} repeated)",
                    wanted.len() - unique.len()
                ),
            ));
        }
        let mut by_id: BTreeMap<String, MountedMod> = std::mem::take(&mut self.mods)
            .into_iter()
            .map(|m| (m.id.clone(), m))
            .collect();
        self.mods = wanted.iter().filter_map(|id| by_id.remove(*id)).collect();
        self.rebuild();
        Ok(())
    }

    /// The mod ids in load order (first = base).
    #[must_use]
    pub fn order(&self) -> Vec<String> {
        self.mods.iter().map(|m| m.id.clone()).collect()
    }

    /// Every mounted mod, in load order.
    #[must_use]
    pub fn mods(&self) -> Vec<ModInfo> {
        (0..self.mods.len()).map(|i| self.info(i)).collect()
    }

    /// One mounted mod.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<ModInfo> {
        self.position(id).ok().map(|i| self.info(i))
    }

    /// Whether nothing is mounted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mods.is_empty()
    }

    /// Bumped by every mount, unmount and order change (a cheap "did the set change" check).
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Every path more than one mod holds, sorted by path.
    #[must_use]
    pub fn conflicts(&self) -> &[ModConflict] {
        &self.conflicts
    }

    /// Number of files `mods://` serves.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.index.len()
    }

    /// Every served path with the mod that serves it, sorted by path.
    pub fn files(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.index
            .iter()
            .map(|(path, &i)| (path.as_str(), self.mods[i].id.as_str()))
    }

    /// The mod that serves `path`, if any.
    pub fn owner(&self, path: &str) -> Result<Option<&str>, ModsError> {
        let rel = mods_relative(path)?;
        Ok(self.index.get(&rel).map(|&i| self.mods[i].id.as_str()))
    }

    /// Whether `path` is a served file or a directory holding one.
    #[must_use]
    pub fn exists(&self, path: &str) -> bool {
        let Ok(rel) = mods_relative(path) else {
            return false;
        };
        if rel.is_empty() || self.index.contains_key(&rel) {
            return true;
        }
        let prefix = format!("{rel}/");
        self.index
            .range(prefix.clone()..)
            .next()
            .is_some_and(|(p, _)| p.starts_with(&prefix))
    }

    /// Reads the file `mods://` serves at `path`.
    pub fn read(&self, path: &str) -> Result<Vec<u8>, ModsError> {
        let rel = mods_relative(path)?;
        let _span = tracing::trace_span!("mods_vfs.read", path = %rel).entered();
        let &at = self.index.get(&rel).ok_or_else(|| {
            ModsError::new(
                ModsErrorCode::NotFound,
                format!("mods://{rel}: no mounted mod holds it"),
            )
        })?;
        read_from(&self.mods[at], &rel)
    }

    /// Reads `path` from mod `id` itself, whether or not a later mod overrides it.
    pub fn read_from_mod(&self, id: &str, path: &str) -> Result<Vec<u8>, ModsError> {
        let rel = mods_relative(path)?;
        let at = self.position(id)?;
        if !self.mods[at].files.contains_key(&rel) {
            return Err(ModsError::new(
                ModsErrorCode::NotFound,
                format!("mods://{rel}: not in mod `{id}`"),
            ));
        }
        read_from(&self.mods[at], &rel)
    }

    /// The size mod `id`'s own file `path` had when the mod mounted: what a read of it returns, or
    /// refuses as changed. Lets a caller check its own limit before it reads (ADR-5870).
    pub fn mod_file_size(&self, id: &str, path: &str) -> Result<u64, ModsError> {
        let rel = mods_relative(path)?;
        let at = self.position(id)?;
        self.mods[at]
            .files
            .get(&rel)
            .map(|record| record.size)
            .ok_or_else(|| {
                ModsError::new(
                    ModsErrorCode::NotFound,
                    format!("mods://{rel}: not in mod `{id}`"),
                )
            })
    }

    /// The paths of mod `id`'s own files, sorted, whether or not a later mod overrides them.
    pub fn mod_paths(&self, id: &str) -> Result<Vec<String>, ModsError> {
        let at = self.position(id)?;
        Ok(self.mods[at].files.keys().cloned().collect())
    }

    /// Every file of mod `id` itself (relative path → bytes), whether or not a later mod overrides
    /// it: what a Lua mod's file set is built from.
    pub fn mod_files(&self, id: &str) -> Result<BTreeMap<String, Vec<u8>>, ModsError> {
        let at = self.position(id)?;
        let m = &self.mods[at];
        m.files
            .keys()
            .map(|rel| read_from(m, rel).map(|bytes| (rel.clone(), bytes)))
            .collect()
    }

    /// Lists a served directory (`mods://` or `mods://textures`), sorted by name.
    ///
    /// An overlay can hold both `cfg` and `cfg/settings.ron` (two mods, or one
    /// [`ModSource::Memory`]); children then win over the file at the directory path, so
    /// descending still lists them.
    pub fn list_dir(&self, path: &str) -> Result<Vec<VfsDirEntry>, ModsError> {
        let rel = mods_relative(path)?;
        let prefix = if rel.is_empty() {
            String::new()
        } else {
            format!("{rel}/")
        };
        let mut out: BTreeMap<String, bool> = BTreeMap::new();
        for (p, _) in self.index.range(prefix.clone()..) {
            let Some(rest) = p.strip_prefix(&prefix) else {
                break;
            };
            match rest.split_once('/') {
                Some((dir, _)) => {
                    out.insert(dir.to_owned(), true);
                }
                None => {
                    out.entry(rest.to_owned()).or_insert(false);
                }
            }
        }
        if !out.is_empty() {
            return Ok(out
                .into_iter()
                .map(|(name, is_dir)| VfsDirEntry { name, is_dir })
                .collect());
        }
        // Nothing lives under `rel`: it is a file, or nothing is there at all.
        if self.index.contains_key(&rel) {
            return Err(ModsError::new(
                ModsErrorCode::NotFound,
                format!("mods://{rel} is a file, not a directory"),
            ));
        }
        if !rel.is_empty() {
            return Err(ModsError::new(
                ModsErrorCode::NotFound,
                format!("mods://{rel}: no such directory"),
            ));
        }
        Ok(Vec::new())
    }

    /// Always refused: `mods://` is read-only. (Validates the path first, so a bad path reports
    /// its own code.)
    pub fn write(&self, path: &str, data: &[u8]) -> Result<(), ModsError> {
        let rel = mods_relative(path)?;
        let _ = data;
        Err(ModsError::new(
            ModsErrorCode::ReadOnly,
            format!("mods://{rel}: mods:// is read-only"),
        ))
    }

    /// The content-set hash of the mounted mods in load order (module docs). The empty set has a
    /// hash too.
    #[must_use]
    pub fn content_set_hash(&self) -> ContentSetHash {
        content_set_hash(self.mods.iter().map(|m| &m.content_hash))
    }

    /// The lobby data entry for the mounted set: ([`MODS_LOBBY_KEY`], 64 hex digits). Put it in the
    /// lobby's data when creating it (`LobbySpec::data`) or with `Lobbies::set_data`, and filter
    /// lobby lists on it with a string-equal filter.
    #[must_use]
    pub fn lobby_entry(&self) -> (&'static str, String) {
        (MODS_LOBBY_KEY, self.content_set_hash().to_hex())
    }

    fn position(&self, id: &str) -> Result<usize, ModsError> {
        self.mods
            .iter()
            .position(|m| m.id == id)
            .ok_or_else(|| ModsError::new(ModsErrorCode::UnknownMod, format!("no mod `{id}` is mounted")))
    }

    fn info(&self, i: usize) -> ModInfo {
        let m = &self.mods[i];
        let overridden = m.files.keys().filter(|p| self.index.get(*p) != Some(&i)).count();
        ModInfo {
            id: m.id.clone(),
            order: i,
            kind: m.kind,
            origin: m.origin.clone(),
            files: m.files.len(),
            bytes: m.bytes,
            overridden,
            content_hash: m.content_hash,
            version: m.version.clone(),
        }
    }

    fn rebuild(&mut self) {
        let mut holders: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for (i, m) in self.mods.iter().enumerate() {
            for path in m.files.keys() {
                holders.entry(path.as_str()).or_default().push(i);
            }
        }
        let mut index = BTreeMap::new();
        let mut conflicts = Vec::new();
        for (path, list) in holders {
            let Some(&winner) = list.last() else {
                continue;
            };
            index.insert(path.to_owned(), winner);
            if list.len() > 1 {
                conflicts.push(ModConflict {
                    path: path.to_owned(),
                    winner: self.mods[winner].id.clone(),
                    overridden: list[..list.len() - 1]
                        .iter()
                        .map(|&i| self.mods[i].id.clone())
                        .collect(),
                });
            }
        }
        self.index = index;
        self.conflicts = conflicts;
        self.revision = self.revision.wrapping_add(1);
    }
}

/// Whether `meta` is a symbolic link or (Windows) any reparse point such as a junction.
fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        meta.file_type().is_symlink() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

fn io_error(path: &Path, e: &io::Error) -> ModsError {
    ModsError::new(ModsErrorCode::Io, format!("{}: {e}", path.display()))
}

fn build_mod(id: &str, source: ModSource, limits: &ModsLimits) -> Result<MountedMod, ModsError> {
    let kind = source.kind();
    let mut files = BTreeMap::new();
    let mut bytes = 0u64;
    let mut add = |rel: String,
                   size: u64,
                   hash: [u8; 32],
                   identity: Option<(u64, u64)>,
                   files: &mut BTreeMap<String, FileRecord>| {
        if files.len() >= limits.max_files_per_mod {
            return Err(ModsError::new(
                ModsErrorCode::TooLarge,
                format!("mod `{id}` holds more than {} files", limits.max_files_per_mod),
            ));
        }
        if size > limits.max_file_bytes {
            return Err(ModsError::new(
                ModsErrorCode::TooLarge,
                format!(
                    "mods://{rel} (mod `{id}`) is {size} bytes, more than {}",
                    limits.max_file_bytes
                ),
            ));
        }
        bytes = bytes.saturating_add(size);
        if bytes > limits.max_mod_bytes {
            return Err(ModsError::new(
                ModsErrorCode::TooLarge,
                format!("mod `{id}` holds more than {} bytes", limits.max_mod_bytes),
            ));
        }
        if files
            .insert(rel.clone(), FileRecord { size, hash, identity })
            .is_some()
        {
            return Err(ModsError::new(
                ModsErrorCode::PathInvalid,
                format!("mod `{id}` holds mods://{rel} twice"),
            ));
        }
        Ok(())
    };
    let (store, origin) = match source {
        ModSource::Dir(root) => {
            let canonical = fs::canonicalize(&root).map_err(|e| io_error(&root, &e))?;
            if !canonical.is_dir() {
                return Err(ModsError::new(
                    ModsErrorCode::Io,
                    format!("{}: not a folder", root.display()),
                ));
            }
            let mut found = Vec::new();
            walk(&canonical, "", 0, limits, &mut found)?;
            for (rel, host, seen) in found {
                let Hashed { size, hash, identity } = hash_file(&host, seen)?;
                add(rel, size, hash, identity, &mut files)?;
            }
            (Store::Dir(canonical.clone()), canonical.display().to_string())
        }
        ModSource::Pak(path) => {
            let mut reader = PakReader::open(&path)
                .map_err(|e| ModsError::new(ModsErrorCode::Pak, format!("{}: {e}", path.display())))?;
            let max_entry = limits.max_file_bytes.min(limits.max_mod_bytes);
            reader.set_max_entry_bytes(max_entry);
            // Every read of a mod is a copy, so the mapping buys nothing, and a Workshop client
            // rewriting the pak in place would fault a borrowed read (SIGBUS): read through the
            // file handle instead (ADR-5869).
            reader.set_zero_copy(false);
            // What the table of contents declares, before any entry is decompressed: the mount
            // hashes every file, so `add` below would only see a size after allocating it.
            // `max_entry` is the per-file cap (the smaller of the file and mod budgets), so this
            // one loop refuses every oversized entry; a second check against `max_file_bytes`
            // alone could never fire (rc1-rev finding 8).
            let mut declared = 0_u64;
            for entry in reader.manifest() {
                let size = reader.entry(entry.id).map_or(0, |e| e.size);
                if size > max_entry {
                    return Err(ModsError::new(
                        ModsErrorCode::TooLarge,
                        format!(
                            "mod `{id}`: a pak entry declares {size} bytes for {}, more than {max_entry}",
                            entry.path
                        ),
                    ));
                }
                declared = declared.saturating_add(size);
            }
            if declared > limits.max_mod_bytes {
                return Err(ModsError::new(
                    ModsErrorCode::TooLarge,
                    format!(
                        "mod `{id}` declares {declared} bytes, more than {}",
                        limits.max_mod_bytes
                    ),
                ));
            }
            let mut ids = BTreeMap::new();
            for entry in reader.manifest() {
                let rel = mods_relative(&entry.path).map_err(|e| ModsError {
                    code: e.code,
                    message: format!("pak {}: {}", path.display(), e.message),
                })?;
                if rel.is_empty() {
                    return Err(ModsError::new(
                        ModsErrorCode::PathInvalid,
                        format!("pak {}: a manifest entry has no file name", path.display()),
                    ));
                }
                let data = reader
                    .read_decompressed(entry.id)
                    .map_err(|e| ModsError::new(ModsErrorCode::Pak, format!("{}: {e}", path.display())))?;
                add(
                    rel.clone(),
                    data.len() as u64,
                    *blake3::hash(&data).as_bytes(),
                    None,
                    &mut files,
                )?;
                ids.insert(rel, entry.id);
            }
            (
                Store::Pak {
                    reader: Arc::new(reader),
                    ids: Arc::new(ids),
                },
                path.display().to_string(),
            )
        }
        ModSource::Memory(map) => {
            let mut normalized = BTreeMap::new();
            for (path, data) in map {
                let rel = mods_relative(&path)?;
                if rel.is_empty() {
                    return Err(ModsError::new(
                        ModsErrorCode::PathInvalid,
                        "a file needs a name (the empty path is the mount root)",
                    ));
                }
                add(
                    rel.clone(),
                    data.len() as u64,
                    *blake3::hash(&data).as_bytes(),
                    None,
                    &mut files,
                )?;
                normalized.insert(rel, data);
            }
            (Store::Memory(Arc::new(normalized)), String::new())
        }
    };
    let content_hash = mod_content_hash(files.iter().map(|(p, r)| (p.as_str(), r.size, &r.hash)));
    Ok(MountedMod {
        id: id.to_owned(),
        kind,
        origin,
        store,
        files: Arc::new(files),
        bytes,
        content_hash,
        version: String::new(),
    })
}

/// The version text a mod declares (ADR-7259): the `version` field of the `luamod.ron` at its root
/// (spec 84 § 3), read once at mount. Only that field is read, so any manifest version or shape
/// that has it works; a mod without the file, or one whose file does not parse, declares none (the
/// Lua host reports a broken manifest itself). Larger than 64 KiB is not a manifest.
fn declared_version(m: &MountedMod) -> String {
    #[derive(serde::Deserialize)]
    struct VersionProbe {
        // A plain string, as `luamod.ron` writes it (an `Option` would need `Some("...")` in RON).
        #[serde(default)]
        version: String,
    }
    const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
    if m.files
        .get(MOD_VERSION_FILE)
        .is_none_or(|r| r.size > MAX_MANIFEST_BYTES)
    {
        return String::new();
    }
    let Ok(bytes) = read_from(m, MOD_VERSION_FILE) else {
        return String::new();
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return String::new();
    };
    let text = text.trim_start_matches('\u{feff}').trim_start();
    // A struct name before the parenthesis (`LuaModManifest(...)`) is allowed and ignored.
    let body = text.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '_');
    let body = if body.starts_with('(') { body } else { text };
    match ron::from_str::<VersionProbe>(body) {
        Ok(VersionProbe { version }) => clean_version(&version),
        Err(_) => String::new(),
    }
}

/// Version text as the manifest carries it: trimmed, no control characters, at most
/// [`MAX_MOD_VERSION_BYTES`] cut on a character boundary.
fn clean_version(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.trim().chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > MAX_MOD_VERSION_BYTES {
            break;
        }
        out.push(c);
    }
    out.trim_end().to_owned()
}

/// A file seen by the walk: relative path, host path, and its (device, inode) on Unix.
type Found = (String, PathBuf, Option<(u64, u64)>);

/// Collects every regular file below `dir`, refusing links.
fn walk(
    dir: &Path,
    rel: &str,
    depth: usize,
    limits: &ModsLimits,
    out: &mut Vec<Found>,
) -> Result<(), ModsError> {
    if depth > limits.max_depth {
        return Err(ModsError::new(
            ModsErrorCode::TooLarge,
            format!("{}: folders nest deeper than {}", dir.display(), limits.max_depth),
        ));
    }
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| io_error(dir, &e))?
        .collect::<Result<_, _>>()
        .map_err(|e| io_error(dir, &e))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let host = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(ModsError::new(
                ModsErrorCode::PathInvalid,
                format!("{}: the name is not UTF-8", host.display()),
            ));
        };
        let child = if rel.is_empty() {
            name.to_owned()
        } else {
            format!("{rel}/{name}")
        };
        let checked = mods_relative(&child)?;
        if checked != child {
            return Err(ModsError::new(
                ModsErrorCode::PathInvalid,
                format!("{}: `{name}` is not a valid mods:// name", host.display()),
            ));
        }
        let meta = fs::symlink_metadata(&host).map_err(|e| io_error(&host, &e))?;
        if is_link(&meta) {
            return Err(ModsError::new(
                ModsErrorCode::Symlink,
                format!("{}: links are not allowed inside a mod", host.display()),
            ));
        }
        if meta.is_dir() {
            walk(&host, &child, depth + 1, limits, out)?;
        } else if meta.is_file() {
            if out.len() >= limits.max_files_per_mod {
                return Err(ModsError::new(
                    ModsErrorCode::TooLarge,
                    format!("{}: more than {} files", dir.display(), limits.max_files_per_mod),
                ));
            }
            out.push((child, host, identity(&meta)));
        } else {
            return Err(ModsError::new(
                ModsErrorCode::Io,
                format!("{}: not a regular file or folder", host.display()),
            ));
        }
    }
    Ok(())
}

/// A file's (device, inode) on Unix; `None` elsewhere.
fn identity(meta: &fs::Metadata) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Some((meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// Opens `path` for reading as a regular file. On Unix the last component is opened without
/// following a link (`O_NOFOLLOW`) and without blocking (`O_NONBLOCK`, so a FIFO swapped in cannot
/// hang the call), and the opened file must be `expected` (device, inode) when given: a file or a
/// folder swapped in between the link check and the open is refused.
fn open_regular(
    path: &Path,
    expected: Option<(u64, u64)>,
) -> Result<(fs::File, Option<(u64, u64)>), ModsError> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|e| {
        #[cfg(unix)]
        if e.raw_os_error() == Some(libc::ELOOP) {
            return ModsError::new(
                ModsErrorCode::Symlink,
                format!("{}: a link is not allowed inside a mod", path.display()),
            );
        }
        if e.kind() == io::ErrorKind::NotFound {
            return ModsError::new(
                ModsErrorCode::Changed,
                format!("{}: the file was removed after its mod mounted", path.display()),
            );
        }
        io_error(path, &e)
    })?;
    let meta = file.metadata().map_err(|e| io_error(path, &e))?;
    if !meta.is_file() {
        return Err(ModsError::new(
            ModsErrorCode::Io,
            format!("{}: not a regular file", path.display()),
        ));
    }
    let opened = identity(&meta);
    if expected.is_some() && opened != expected {
        return Err(ModsError::new(
            ModsErrorCode::Changed,
            format!("{}: another file was put in its place", path.display()),
        ));
    }
    Ok((file, opened))
}

/// What `hash_file` read: size, BLAKE3, and the (device, inode) on Unix.
struct Hashed {
    size: u64,
    hash: [u8; 32],
    identity: Option<(u64, u64)>,
}

fn hash_file(path: &Path, seen: Option<(u64, u64)>) -> Result<Hashed, ModsError> {
    let (mut file, identity) = open_regular(path, seen)?;
    let mut hasher = blake3::Hasher::new();
    let size = io::copy(&mut file, &mut hasher).map_err(|e| io_error(path, &e))?;
    Ok(Hashed {
        size,
        hash: *hasher.finalize().as_bytes(),
        identity,
    })
}

/// The host path of `rel` below `root`, refused when any component below the root is a link.
fn confined_host_path(root: &Path, rel: &str) -> Result<PathBuf, ModsError> {
    let mut walk = root.to_path_buf();
    for segment in rel.split('/') {
        walk.push(segment);
        let meta = fs::symlink_metadata(&walk).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                ModsError::new(
                    ModsErrorCode::Changed,
                    format!("mods://{rel}: the file was removed after its mod mounted"),
                )
            } else {
                io_error(&walk, &e)
            }
        })?;
        if is_link(&meta) {
            return Err(ModsError::new(
                ModsErrorCode::Symlink,
                format!("{}: a link appeared after the mod mounted", walk.display()),
            ));
        }
    }
    Ok(walk)
}

fn read_from(m: &MountedMod, rel: &str) -> Result<Vec<u8>, ModsError> {
    let record = m.files.get(rel).ok_or_else(|| {
        ModsError::new(
            ModsErrorCode::NotFound,
            format!("mods://{rel}: not in mod `{}`", m.id),
        )
    })?;
    let data = match &m.store {
        Store::Dir(root) => {
            let host = confined_host_path(root, rel)?;
            let (file, _) = open_regular(&host, record.identity)?;
            let mut data = Vec::with_capacity(usize::try_from(record.size).unwrap_or(0).min(64 << 20));
            // One byte past the recorded size is enough to see that the file grew.
            file.take(record.size.saturating_add(1))
                .read_to_end(&mut data)
                .map_err(|e| io_error(&host, &e))?;
            data
        }
        Store::Pak { reader, ids } => {
            let id = ids.get(rel).ok_or_else(|| {
                ModsError::new(ModsErrorCode::NotFound, format!("mods://{rel}: not in the pak"))
            })?;
            reader
                .read_decompressed(*id)
                .map_err(|e| ModsError::new(ModsErrorCode::Pak, format!("mods://{rel}: {e}")))?
        }
        Store::Memory(map) => map.get(rel).cloned().ok_or_else(|| {
            ModsError::new(
                ModsErrorCode::NotFound,
                format!("mods://{rel}: not in mod `{}`", m.id),
            )
        })?,
    };
    if data.len() as u64 != record.size || *blake3::hash(&data).as_bytes() != record.hash {
        return Err(ModsError::new(
            ModsErrorCode::Changed,
            format!(
                "mods://{rel} (mod `{}`) changed after the mod mounted; remount it",
                m.id
            ),
        ));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(files: &[(&str, &str)]) -> ModSource {
        ModSource::Memory(
            files
                .iter()
                .map(|(p, t)| ((*p).to_owned(), t.as_bytes().to_vec()))
                .collect(),
        )
    }

    #[test]
    fn relative_paths_are_confined() {
        assert_eq!(mods_relative("mods://a/./b.lua").as_deref(), Ok("a/b.lua"));
        assert_eq!(mods_relative("a/b.lua").as_deref(), Ok("a/b.lua"));
        assert_eq!(mods_relative("mods://").as_deref(), Ok(""));
        for escape in [
            "mods://../x",
            "a/../../x",
            "/etc/passwd",
            "mods:///etc/passwd",
            "C:/x",
            "c:x",
            "\\x",
            "a\\..\\b",
        ] {
            assert_eq!(
                mods_relative(escape).map_err(|e| e.code),
                Err(ModsErrorCode::PathEscape),
                "{escape}"
            );
        }
        for invalid in ["assets://x", "a//b", "a\\b", "a\0b", "nul.txt", "ab:c"] {
            assert_eq!(
                mods_relative(invalid).map_err(|e| e.code),
                Err(ModsErrorCode::PathInvalid),
                "{invalid}"
            );
        }
    }

    #[test]
    fn later_mods_override_and_conflicts_are_reported() {
        let mut vfs = ModsVfs::new();
        vfs.mount("base", mem(&[("a.txt", "base a"), ("b.txt", "base b")]))
            .expect("base");
        vfs.mount("patch", mem(&[("a.txt", "patch a"), ("c.txt", "patch c")]))
            .expect("patch");
        assert_eq!(vfs.read("mods://a.txt").as_deref(), Ok(&b"patch a"[..]));
        assert_eq!(vfs.read("mods://b.txt").as_deref(), Ok(&b"base b"[..]));
        assert_eq!(vfs.read_from_mod("base", "a.txt").as_deref(), Ok(&b"base a"[..]));
        assert_eq!(
            vfs.conflicts(),
            &[ModConflict {
                path: "a.txt".into(),
                winner: "patch".into(),
                overridden: vec!["base".into()],
            }]
        );
        assert_eq!(vfs.get("base").map(|m| m.overridden), Some(1));
        vfs.set_order(&["patch", "base"]).expect("order");
        assert_eq!(vfs.read("a.txt").as_deref(), Ok(&b"base a"[..]));
        assert_eq!(vfs.owner("a.txt"), Ok(Some("base")));
    }

    #[test]
    fn hashes_depend_on_content_and_order_not_ids() {
        let mut one = ModsVfs::new();
        one.mount("x", mem(&[("f", "1")])).expect("x");
        one.mount("y", mem(&[("g", "2")])).expect("y");
        let mut renamed = ModsVfs::new();
        renamed.mount("other", mem(&[("f", "1")])).expect("other");
        renamed.mount("names", mem(&[("g", "2")])).expect("names");
        assert_eq!(one.content_set_hash(), renamed.content_set_hash());
        let before = one.content_set_hash();
        one.set_order(&["y", "x"]).expect("order");
        assert_ne!(one.content_set_hash(), before);
        assert_eq!(ContentSetHash::from_hex(&before.to_hex()), Some(before));
        assert_ne!(ModsVfs::new().content_set_hash(), before);
    }

    #[test]
    fn order_must_be_a_permutation() {
        let mut vfs = ModsVfs::new();
        vfs.mount("a", mem(&[("f", "1")])).expect("a");
        vfs.mount("b", mem(&[("g", "1")])).expect("b");
        for bad in [&["a"][..], &["a", "a"], &["a", "b", "c"], &["a", "c"]] {
            assert_eq!(
                vfs.set_order(bad).map_err(|e| e.code),
                Err(ModsErrorCode::OrderInvalid),
                "{bad:?}"
            );
        }
        assert_eq!(vfs.order(), ["a", "b"]);
    }

    #[test]
    fn ids_writes_and_lists() {
        let mut vfs = ModsVfs::new();
        assert_eq!(
            vfs.mount("Bad Id", mem(&[])).map_err(|e| e.code),
            Err(ModsErrorCode::IdInvalid)
        );
        vfs.mount("a", mem(&[("t/x.png", "1"), ("t/y/z.png", "2"), ("r.txt", "3")]))
            .expect("a");
        assert_eq!(
            vfs.mount("a", mem(&[])).map_err(|e| e.code),
            Err(ModsErrorCode::Duplicate)
        );
        assert_eq!(
            vfs.write("mods://r.txt", b"x").map_err(|e| e.code),
            Err(ModsErrorCode::ReadOnly)
        );
        let names: Vec<(String, bool)> = vfs
            .list_dir("mods://t")
            .expect("list")
            .into_iter()
            .map(|e| (e.name, e.is_dir))
            .collect();
        assert_eq!(names, [("x.png".to_owned(), false), ("y".to_owned(), true)]);
        assert!(vfs.exists("mods://t/y"));
        assert!(!vfs.exists("mods://t/q"));
        assert_eq!(
            vfs.read("mods://nope").map_err(|e| e.code),
            Err(ModsErrorCode::NotFound)
        );
        assert_eq!(
            vfs.unmount("zz").map_err(|e| e.code),
            Err(ModsErrorCode::UnknownMod)
        );
        let codes: BTreeSet<&str> = ModsErrorCode::ALL.iter().map(|c| c.code()).collect();
        assert_eq!(codes.len(), ModsErrorCode::ALL.len());
    }
    /// sec-review-assets-agent (ADR-5869) finding 5: a mounted mod pak reads through its file
    /// handle, never a borrow of the mapping.
    #[test]
    fn a_mounted_mod_pak_is_not_read_from_the_mapping() {
        use crate::{FPAK_BLOB_ALIGNMENT, FPAK_VERSION, ManifestEntry, PakEntry, PakToc, encode_pak_header};
        let dir = regia_test_utils::unique_temp_dir("mods-pak-mapping");
        let path = dir.join("m.fpak");
        let mut data = vec![0_u8; 60];
        data.resize(data.len().next_multiple_of(FPAK_BLOB_ALIGNMENT as usize), 0);
        let id = AssetId(regia_core::id::Guid(7));
        let offset = data.len() as u64;
        data.extend_from_slice(b"hello");
        let toc = PakToc {
            entries: vec![PakEntry {
                id,
                type_tag: regia_core::id::TypePathHash::of("regia_asset::Blob"),
                offset,
                size: 5,
                compressed_size: 5,
                block_table_offset: 0,
            }],
            manifest: vec![ManifestEntry {
                path: "a.txt".to_owned(),
                id,
                type_path: "regia_asset::Blob".into(),
            }],
            dependencies: Vec::new(),
        };
        let toc_offset = data.len() as u64;
        let toc_bytes = postcard::to_allocvec(&toc).expect("toc");
        data.extend_from_slice(&toc_bytes);
        let hash = *blake3::hash(&data[..toc_offset as usize]).as_bytes();
        let header = encode_pak_header(FPAK_VERSION, 0, toc_offset, toc_bytes.len() as u64, &hash);
        data[..60].copy_from_slice(&header);
        std::fs::write(&path, data).expect("write pak");
        assert!(
            PakReader::open(&path).expect("open").zero_copy(),
            "a plain open maps the pak"
        );
        let mut vfs = ModsVfs::default();
        vfs.mount("m", ModSource::Pak(path)).expect("mount");
        assert_eq!(vfs.read("mods://a.txt").expect("read"), b"hello");
        let Store::Pak { reader, .. } = &vfs.mods[0].store else {
            panic!("a pak mod has a pak store");
        };
        assert!(!reader.zero_copy(), "a mod pak must not be read from the mapping");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spec 77 § 4.1 (ADR-7259): a mod's version text comes from its own `luamod.ron` `version`,
    /// as a folder, a pak or an in-memory map; a mod that declares none has an empty version, and
    /// a mod manager can set one (a Workshop item's version) afterwards.
    #[test]
    fn a_mod_reports_the_version_its_luamod_declares() {
        let mut vfs = ModsVfs::new();
        let manifest = "(\n    format_version: 1,\n    id: \"speedrun\",\n    name: \"Speedrun\",\n    \
                        version: \" 1.2.0 \",\n    entry: \"main.lua\",\n    api_version: 1,\n)";
        vfs.mount("speedrun", mem(&[("luamod.ron", manifest), ("main.lua", "")]))
            .expect("mount");
        assert_eq!(vfs.get("speedrun").expect("mounted").version, "1.2.0");
        vfs.mount("plain", mem(&[("levels/a.level.ron", "()")]))
            .expect("mount");
        assert_eq!(
            vfs.get("plain").expect("mounted").version,
            "",
            "no luamod.ron, no version"
        );
        vfs.mount("broken", mem(&[("luamod.ron", "(version: ")]))
            .expect("a bad manifest still mounts");
        assert_eq!(vfs.get("broken").expect("mounted").version, "");
        vfs.mount("nested", mem(&[("sub/luamod.ron", "(version: \"9\")")]))
            .expect("mount");
        assert_eq!(
            vfs.get("nested").expect("mounted").version,
            "",
            "only the mod's root manifest counts"
        );
        let long = format!("(version: \"{}\")", "é".repeat(40));
        vfs.mount("long", mem(&[("luamod.ron", long.as_str())]))
            .expect("mount");
        let cut = vfs.get("long").expect("mounted").version;
        assert!(cut.len() <= MAX_MOD_VERSION_BYTES, "{} bytes", cut.len());
        assert_eq!(
            cut,
            "é".repeat(MAX_MOD_VERSION_BYTES / 2),
            "cut on a character boundary"
        );

        let revision = vfs.revision();
        vfs.set_mod_version("plain", "2024.1").expect("set");
        assert_eq!(vfs.get("plain").expect("mounted").version, "2024.1");
        assert_ne!(vfs.revision(), revision, "a version change is a set change");
        assert_eq!(
            vfs.set_mod_version("nope", "1").map_err(|e| e.code),
            Err(ModsErrorCode::UnknownMod)
        );

        let dir = regia_test_utils::unique_temp_dir("mods-version-dir");
        std::fs::write(dir.join("luamod.ron"), "(version: \"0.3\", id: \"x\")").expect("write");
        vfs.mount("folder", ModSource::Dir(dir.clone())).expect("mount");
        assert_eq!(vfs.get("folder").expect("mounted").version, "0.3");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
