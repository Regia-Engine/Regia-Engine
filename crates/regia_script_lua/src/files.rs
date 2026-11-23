// SPDX-License-Identifier: MPL-2.0
//! A mod's files: relative path → text, read once under a size cap before any Lua runs.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

thread_local! {
    /// File bodies [`ModFiles::from_dir`] has read on this thread.
    static MOD_FILE_BODY_READS: Cell<u64> = const { Cell::new(0) };
}

/// How many mod file bodies [`ModFiles::from_dir`] has read on the calling thread.
///
/// A directory watch that sees an unchanged stamp does not add to this count. The count is per
/// thread (the read runs on the caller's thread), so a test's count does not pick up another
/// test's reads.
#[must_use]
pub fn mod_file_body_reads() -> u64 {
    MOD_FILE_BODY_READS.with(Cell::get)
}

/// Zeroes [`mod_file_body_reads`] for the calling thread.
pub fn reset_mod_file_body_reads() {
    MOD_FILE_BODY_READS.with(|count| count.set(0));
}

/// Directory levels `from_dir` walks below the mod root.
const MAX_DIR_DEPTH: usize = 8;

/// Size and modification time of one file `from_dir` read. Compared before a later read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    path: String,
    len: u64,
    /// Nanoseconds since the Unix epoch, or 0 when the platform has no modification time.
    mtime_nanos: u128,
    /// The inode number on Unix (0 elsewhere): an atomic save renames a new file over the old
    /// one, which a same-size write inside the timestamp granularity would otherwise hide.
    file_id: u64,
}

/// A mod's files. Paths use `/`, are relative to the mod root and never contain `..`.
///
/// Equality is the file text and the source directory. The directory stamp is bookkeeping
/// for the watch and is not part of it.
#[derive(Debug, Clone, Default)]
pub struct ModFiles {
    files: BTreeMap<String, String>,
    /// The directory `from_dir` read: hot reload reads it again.
    root: Option<PathBuf>,
    /// Size and mtime of the files [`from_dir`](Self::from_dir) read. `None` for a map or a VFS read.
    stamp: Option<Vec<FileStamp>>,
    /// Where the mod is mounted ([`mount`](Self::mount), ADR-7881).
    mount: Option<String>,
}

impl PartialEq for ModFiles {
    fn eq(&self, other: &Self) -> bool {
        self.files == other.files && self.root == other.root && self.mount == other.mount
    }
}

/// Longest mount name [`ModFiles::with_mount`] takes, in bytes.
pub const MAX_MOUNT_BYTES: usize = 256;

impl Eq for ModFiles {}

/// Why a mod's files could not be read.
#[derive(Debug, thiserror::Error)]
pub enum FilesError {
    /// A read or directory walk failed.
    #[error("reading {path}: {source}")]
    Io {
        /// The path that failed.
        path: String,
        /// The IO error.
        source: std::io::Error,
    },
    /// The files add up to more than the cap.
    #[error("the mod's files are {bytes} bytes, more than the {max} allowed")]
    TooLarge {
        /// Bytes seen so far.
        bytes: u64,
        /// The cap.
        max: u64,
    },
    /// A file is not UTF-8 text.
    #[error("{0} is not UTF-8 text")]
    NotText(String),
}

impl From<FilesError> for crate::ModError {
    fn from(error: FilesError) -> Self {
        let code = match error {
            FilesError::TooLarge { .. } => crate::LuaErrorCode::TooLarge,
            FilesError::Io { .. } | FilesError::NotText(_) => crate::LuaErrorCode::Io,
        };
        crate::ModError::new(code, error.to_string())
    }
}

impl ModFiles {
    /// Files from `(path, text)` pairs (tests, packed mods, Android VFS reads). Unsafe paths
    /// (absolute, `..`, backslashes) are dropped with a warning.
    ///
    /// ```
    /// use regia_script_lua::ModFiles;
    ///
    /// let files = ModFiles::from_map([("main.lua", "print('hi')"), ("../escape.lua", "evil()")]);
    /// assert_eq!(files.get("main.lua"), Some("print('hi')"));
    /// // A path that climbs out of the mod is dropped.
    /// assert_eq!(files.get("../escape.lua"), None);
    /// assert_eq!(files.paths().count(), 1);
    /// ```
    pub fn from_map<P: Into<String>, T: Into<String>>(entries: impl IntoIterator<Item = (P, T)>) -> Self {
        let files = entries
            .into_iter()
            .map(|(p, t)| (p.into(), t.into()))
            .filter(|(p, _)| {
                let safe = is_safe_relative(p);
                if !safe {
                    tracing::warn!(target: "regia_script_lua", path = %p, "a path that leaves the mod was dropped from its files");
                }
                safe
            })
            .collect();
        Self {
            files,
            root: None,
            stamp: None,
            mount: None,
        }
    }

    /// Reads `luamod.ron` and every `.lua` file under `dir` (desktop), at most `MAX_DIR_DEPTH` (8)
    /// levels deep, refusing more than `max_bytes` in total. Symbolic links are not followed.
    pub fn from_dir(dir: &Path, max_bytes: u64) -> Result<Self, FilesError> {
        let mut files = BTreeMap::new();
        let mut total = 0u64;
        let mut stamp = Vec::new();
        walk(dir, "", 0, max_bytes, &mut total, &mut files, &mut stamp)?;
        stamp.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Self {
            files,
            root: Some(dir.to_path_buf()),
            stamp: Some(stamp),
            mount: Some(dir_mount(dir)?),
        })
    }

    /// Whether `dir` still has the size and modification time recorded when these files were read.
    ///
    /// `Ok(false)` when this value has no stamp, or the directory's listing differs. An IO error
    /// is returned so the caller can fall back to reading the bodies. Symbolic links are not
    /// followed. On Unix the stamp also holds the inode, so a save that renames a new file over
    /// the old one is seen even with the same size and time. An in-place same-size rewrite that
    /// does not change the modification time (inside the file system's timestamp granularity)
    /// is not detected until that time changes.
    pub(crate) fn stamp_matches_dir(&self, dir: &Path) -> Result<bool, FilesError> {
        let Some(stamp) = &self.stamp else {
            return Ok(false);
        };
        let mut peeked = Vec::new();
        peek_walk(dir, "", 0, &mut peeked)?;
        peeked.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(stamp == &peeked)
    }

    /// The Lua files (`luamod.ron` and `*.lua`) of mod `id` mounted at `mods://` (ADR-2418): the
    /// mod's own files, whether or not a later mod overrides them, read through the confined mount.
    /// Works the same on every platform, Android included. A read the mount refuses is
    /// [`FilesError::Io`] with the `mods_*` code in the message.
    pub fn from_mods_vfs(
        mods: &regia_asset::mods_vfs::ModsVfs,
        id: &str,
        max_bytes: u64,
    ) -> Result<Self, FilesError> {
        let refused = |path: &str, e: regia_asset::mods_vfs::ModsError| FilesError::Io {
            path: format!("mods://{path} (mod `{id}`)"),
            source: std::io::Error::other(e.to_string()),
        };
        let mut files = BTreeMap::new();
        let mut total = 0u64;
        for path in mods.mod_paths(id).map_err(|e| refused("", e))? {
            let name = path.rsplit('/').next().unwrap_or(&path);
            let wanted = path == crate::LUAMOD_FILE || name.ends_with(".lua");
            if !wanted || !is_safe_relative(&path) {
                continue;
            }
            // The cap before the read: a read returns the whole file (a pak entry decompressed),
            // so a Workshop item's multi-GiB `main.lua` was allocated before it was refused
            // (ADR-5870). A read returns exactly the mount-time size or refuses as changed.
            let size = mods.mod_file_size(id, &path).map_err(|e| refused(&path, e))?;
            if total.saturating_add(size) > max_bytes {
                return Err(FilesError::TooLarge {
                    bytes: total.saturating_add(size),
                    max: max_bytes,
                });
            }
            let bytes = mods.read_from_mod(id, &path).map_err(|e| refused(&path, e))?;
            total = total.saturating_add(bytes.len() as u64);
            if total > max_bytes {
                return Err(FilesError::TooLarge {
                    bytes: total,
                    max: max_bytes,
                });
            }
            let text = String::from_utf8(bytes).map_err(|_| FilesError::NotText(path.clone()))?;
            files.insert(path, text);
        }
        Ok(Self {
            files,
            root: None,
            stamp: None,
            mount: Some(format!("{MODS_MOUNT_PREFIX}{id}")),
        })
    }

    /// Where the mod is mounted: `mods:<mount id>` for [`from_mods_vfs`](Self::from_mods_vfs)
    /// (a Workshop item is `mods:workshop.<item id>`), `dir:<canonical-path hash>` for
    /// [`from_dir`](Self::from_dir), what [`with_mount`](Self::with_mount) set, and `None` for
    /// [`from_map`](Self::from_map). Mounts isolate simultaneous id claims. Frozen luastate v1
    /// keeps manifest ids. A Workshop mount does not receive a v1 manifest entry; a local
    /// `mods:` folder or `.fpak` does (ADR-7951, ADR-7992).
    /// A mount always holds a `:`, which a mod id never does.
    #[must_use]
    pub fn mount(&self) -> Option<&str> {
        self.mount.as_deref()
    }

    /// Binds files the game read itself (an Android VFS read, a packed mod) to a mount for claim
    /// isolation. A `mods:workshop.<item id>` mount cannot restore v1 manifest entries; other
    /// mounts can. `mount` is `<scheme>:<name>`: 1 to
    /// [`MAX_MOUNT_BYTES`] bytes, a `:`, no NUL.
    ///
    /// ```
    /// use regia_script_lua::ModFiles;
    ///
    /// let files = ModFiles::from_map([("main.lua", "")]).with_mount("pack:lights").unwrap();
    /// assert_eq!(files.mount(), Some("pack:lights"));
    /// assert!(ModFiles::from_map([("main.lua", "")]).with_mount("lights").is_err());
    /// ```
    ///
    /// # Errors
    /// `lua_manifest` when `mount` is not of that form.
    pub fn with_mount(mut self, mount: &str) -> Result<Self, crate::ModError> {
        let valid = !mount.is_empty()
            && mount.len() <= MAX_MOUNT_BYTES
            && mount.contains(':')
            && !mount.contains('\0');
        if !valid {
            return Err(crate::ModError::new(
                crate::LuaErrorCode::Manifest,
                format!(
                    "mount `{}` is not `<scheme>:<name>` (1..={MAX_MOUNT_BYTES} bytes, no NUL)",
                    mount.escape_debug()
                ),
            ));
        }
        self.mount = Some(mount.to_owned());
        Ok(self)
    }

    /// Sets the mount without checks (a reload keeps the mod's mount).
    pub(crate) fn set_mount(&mut self, mount: Option<String>) {
        self.mount = mount;
    }

    /// The directory these files were read from, when they came from [`from_dir`](Self::from_dir).
    #[must_use]
    pub fn source_dir(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// The text of `path`, when the mod has it.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&str> {
        self.files.get(path).map(String::as_str)
    }

    /// Every path, sorted.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// Total bytes of all files.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.files.values().map(|t| t.len() as u64).sum()
    }
}

/// The mount prefix of a mod read from `mods://`.
const MODS_MOUNT_PREFIX: &str = "mods:";

/// A fixed-size identity of the canonical directory, including all parents. Hash native path
/// bytes without lossy Unicode conversion; this identity is local to the machine (ADR-7951).
fn dir_mount(dir: &Path) -> Result<String, FilesError> {
    let canonical = dir.canonicalize().map_err(|source| FilesError::Io {
        path: dir.display().to_string(),
        source,
    })?;
    Ok(format!(
        "dir:{}",
        regia_asset::content_hash_hex(canonical.as_os_str().as_encoded_bytes())
    ))
}

fn file_stamp(path: &str, meta: &std::fs::Metadata) -> FileStamp {
    let mtime_nanos = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    #[cfg(unix)]
    let file_id = std::os::unix::fs::MetadataExt::ino(meta);
    #[cfg(not(unix))]
    let file_id = 0;
    FileStamp {
        path: path.to_owned(),
        len: meta.len(),
        mtime_nanos,
        file_id,
    }
}

fn walk(
    dir: &Path,
    prefix: &str,
    depth: usize,
    max_bytes: u64,
    total: &mut u64,
    files: &mut BTreeMap<String, String>,
    stamp: &mut Vec<FileStamp>,
) -> Result<(), FilesError> {
    let io = |source| FilesError::Io {
        path: dir.display().to_string(),
        source,
    };
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(io)?
        .collect::<Result<_, _>>()
        .map_err(io)?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let kind = entry.file_type().map_err(|source| FilesError::Io {
            path: rel.clone(),
            source,
        })?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if depth + 1 < MAX_DIR_DEPTH {
                walk(&entry.path(), &rel, depth + 1, max_bytes, total, files, stamp)?;
            } else {
                tracing::warn!(target: "regia_script_lua", path = %rel, "a directory past the depth limit was not read");
            }
            continue;
        }
        let wanted = rel == crate::LUAMOD_FILE || name.ends_with(".lua");
        if !kind.is_file() || !wanted || !is_safe_relative(&rel) {
            continue;
        }
        // Bytes read count, not the length the listing saw: a file swapped after the listing
        // cannot read past the cap.
        let io = |source| FilesError::Io {
            path: rel.clone(),
            source,
        };
        let Some(file) = open_mod_file(&entry.path()).map_err(io)? else {
            continue;
        };
        let meta = file.metadata().map_err(io)?;
        let mut bytes = Vec::new();
        file.take(max_bytes.saturating_sub(*total).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(io)?;
        *total = total.saturating_add(bytes.len() as u64);
        if *total > max_bytes {
            return Err(FilesError::TooLarge {
                bytes: *total,
                max: max_bytes,
            });
        }
        let text = String::from_utf8(bytes).map_err(|_| FilesError::NotText(rel.clone()))?;
        MOD_FILE_BODY_READS.with(|count| count.set(count.get().saturating_add(1)));
        stamp.push(file_stamp(&rel, &meta));
        files.insert(rel, text);
    }
    Ok(())
}

/// The same file set as [`walk`], recording size and mtime and not reading bodies.
fn peek_walk(dir: &Path, prefix: &str, depth: usize, stamp: &mut Vec<FileStamp>) -> Result<(), FilesError> {
    let io = |source| FilesError::Io {
        path: dir.display().to_string(),
        source,
    };
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(io)?
        .collect::<Result<_, _>>()
        .map_err(io)?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let kind = entry.file_type().map_err(|source| FilesError::Io {
            path: rel.clone(),
            source,
        })?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if depth + 1 < MAX_DIR_DEPTH {
                peek_walk(&entry.path(), &rel, depth + 1, stamp)?;
            }
            continue;
        }
        let wanted = rel == crate::LUAMOD_FILE || name.ends_with(".lua");
        if !kind.is_file() || !wanted || !is_safe_relative(&rel) {
            continue;
        }
        let meta = entry.metadata().map_err(|source| FilesError::Io {
            path: rel.clone(),
            source,
        })?;
        if !meta.file_type().is_file() {
            continue;
        }
        stamp.push(file_stamp(&rel, &meta));
    }
    Ok(())
}

/// Opens a file the listing saw as a regular file; `None` when the path is no longer one. The
/// check is on the opened handle, not the listing (audit v034 net L4): a link swapped in after
/// the listing is not followed, and a FIFO swapped in does not block (Unix opens with
/// `O_NOFOLLOW | O_NONBLOCK`, Windows opens a reparse point itself rather than its target).
fn open_mod_file(path: &Path) -> std::io::Result<Option<std::fs::File>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        /// `FILE_FLAG_OPEN_REPARSE_POINT` (Win32 `CreateFileW` flag).
        const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(OPEN_REPARSE_POINT);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        // `O_NOFOLLOW` on a symbolic link.
        #[cfg(unix)]
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(file.metadata()?.file_type().is_file().then_some(file))
}

/// Whether `path` is a non-empty relative path with `/` separators, no `..`, `.` or empty
/// segments, no backslash and no drive or root prefix, and passes the shared path rule
/// (`regia_core::project_path::check_relative`, ADR-4922): no Windows device name (`nul.lua`,
/// `con/x.lua`) and no segment Windows reads as `..` (`...`).
pub(crate) fn is_safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains(':')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|seg| !seg.is_empty() && seg != ".." && seg != ".")
        && regia_core::project_path::check_relative(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit v034 net L4: the file opened is the file the listing checked. A `.lua` swapped for a
    /// symbolic link after the listing is not read through the link.
    #[cfg(unix)]
    #[test]
    fn a_file_swapped_for_a_symlink_is_not_followed() {
        let dir = regia_test_utils::unique_temp_dir("lua-files-link");
        let outside = dir.join("outside.txt");
        std::fs::write(&outside, "secret").expect("outside file");
        let link = dir.join("main.lua");
        std::os::unix::fs::symlink(&outside, &link).expect("symlink");
        let opened = open_mod_file(&link).expect("no error");
        assert!(opened.is_none(), "the link was followed");
        let plain = dir.join("plain.lua");
        std::fs::write(&plain, "return 1").expect("plain file");
        assert!(open_mod_file(&plain).expect("opens").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `.lua` swapped for a FIFO does not block the loader (opening a FIFO for reading waits for
    /// a writer).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_file_swapped_for_a_fifo_does_not_block() {
        let dir = regia_test_utils::unique_temp_dir("lua-files-fifo");
        let fifo = dir.join("main.lua");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !made.is_ok_and(|s| s.success()) {
            let _ = std::fs::remove_dir_all(&dir);
            return; // no mkfifo on this host
        }
        let opened = open_mod_file(&fifo).expect("no error");
        assert!(opened.is_none(), "a FIFO is not a mod file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsafe_paths_are_dropped_from_maps() {
        let files = ModFiles::from_map([
            ("main.lua", "x"),
            ("../escape.lua", "x"),
            ("/abs.lua", "x"),
            ("c:/drive.lua", "x"),
            ("a\\b.lua", "x"),
            ("lib/./x.lua", "x"),
            ("lib/util.lua", "y"),
        ]);
        assert_eq!(files.paths().collect::<Vec<_>>(), ["lib/util.lua", "main.lua"]);
        assert_eq!(files.total_bytes(), 2);
    }

    #[test]
    fn from_dir_reads_lua_and_the_manifest_only_under_the_cap() {
        let dir = regia_test_utils::unique_temp_dir("lua_mod_files");
        std::fs::create_dir_all(dir.join("lib")).expect("mkdir");
        std::fs::write(dir.join("luamod.ron"), "()").expect("write");
        std::fs::write(dir.join("main.lua"), "return 1").expect("write");
        std::fs::write(dir.join("lib/util.lua"), "return 2").expect("write");
        std::fs::write(dir.join("notes.txt"), "ignored").expect("write");
        let files = ModFiles::from_dir(&dir, 1024).expect("reads");
        assert_eq!(
            files.paths().collect::<Vec<_>>(),
            ["lib/util.lua", "luamod.ron", "main.lua"]
        );
        assert_eq!(files.get("lib/util.lua"), Some("return 2"));
        let too_small = ModFiles::from_dir(&dir, 4);
        assert!(matches!(too_small, Err(FilesError::TooLarge { .. })));
        let code = too_small.map_err(|e| crate::ModError::from(e).code);
        assert_eq!(code.err(), Some(crate::LuaErrorCode::TooLarge));
        let missing =
            ModFiles::from_dir(&dir.join("absent"), 1024).map_err(|e| crate::ModError::from(e).code);
        assert_eq!(missing.err(), Some(crate::LuaErrorCode::Io));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A same-size save that keeps the modification time (an editor's atomic save inside the
    /// file system's timestamp granularity, here forced with `set_modified`) replaces the file:
    /// the stamp's file id sees it (corr-lua-telemetry-matgraph LOW list, ADR-5895). Before the
    /// fix size and mtime matched and the watch kept the old text until the file changed again.
    #[cfg(unix)]
    #[test]
    fn a_same_size_save_by_rename_with_the_same_mtime_is_seen() {
        let dir = regia_test_utils::unique_temp_dir("lua_stamp_rename");
        let main = dir.join("main.lua");
        std::fs::write(&main, "return 1").expect("write");
        let files = ModFiles::from_dir(&dir, 1 << 20).expect("reads");
        assert!(files.stamp_matches_dir(&dir).expect("peek"), "unchanged");
        let mtime = std::fs::metadata(&main)
            .and_then(|m| m.modified())
            .expect("mtime");
        let tmp = dir.join("main.lua.tmp");
        std::fs::write(&tmp, "return 2").expect("write the new text");
        std::fs::File::options()
            .write(true)
            .open(&tmp)
            .and_then(|f| f.set_modified(mtime))
            .expect("same mtime");
        std::fs::rename(&tmp, &main).expect("rename over");
        assert!(
            !files.stamp_matches_dir(&dir).expect("peek"),
            "a replaced file of the same size and mtime is a change"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
