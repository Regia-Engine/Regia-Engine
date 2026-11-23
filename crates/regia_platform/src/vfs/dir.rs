// SPDX-License-Identifier: MPL-2.0
//! [`DirMount`]: a mount backed by a host directory.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{VfsDirEntry, VfsMount, VfsReader};

/// A mount backed by a directory on the host file system (desktop mounts, Android `user://` and
/// `cache://`). The directory does not need to exist until something is written.
///
/// Symbolic links: a writable mount (and a read-only one made with [`DirMount::without_symlinks`])
/// refuses every path that passes through a symbolic link (or, on Windows, any reparse point such
/// as a junction) below its root, with [`io::ErrorKind::PermissionDenied`]: a link planted in a
/// save or cache folder (a restored backup, a synced folder, another local process) must not move
/// reads or writes outside the root. The root itself may be a link. A plain read-only mount
/// (packaged `assets://`, `config://`) follows links: a developer may link a shared asset folder.
#[derive(Debug, Clone)]
pub struct DirMount {
    root: PathBuf,
    writable: bool,
    follow_symlinks: bool,
}

impl DirMount {
    /// A read-only mount of `root` that follows symbolic links (packaged content).
    pub fn read_only(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            writable: false,
            follow_symlinks: true,
        }
    }

    /// A writable mount of `root`. It never follows a symbolic link below `root`.
    pub fn writable(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            writable: true,
            follow_symlinks: false,
        }
    }

    /// This mount, refusing every path through a symbolic link below its root (for a read-only
    /// view of a folder other code or users write, such as `logs://` under the user directory).
    #[must_use]
    pub fn without_symlinks(mut self) -> Self {
        self.follow_symlinks = false;
        self
    }

    /// Whether this mount follows symbolic links below its root.
    pub fn follows_symlinks(&self) -> bool {
        self.follow_symlinks
    }

    /// The host directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The host path of `relative`, refused when this mount does not follow links and an existing
    /// component below the root is a symbolic link or reparse point. A component that does not
    /// exist ends the walk: nothing below it exists either, and the writer creates real
    /// directories. A link swapped in between this check and the open is not caught (a local
    /// race; the writer's temp file is still created exclusively).
    fn checked(&self, relative: &str) -> io::Result<PathBuf> {
        let path = self.resolve(relative);
        if self.follow_symlinks {
            return Ok(path);
        }
        let mut walk = self.root.clone();
        for segment in relative.split('/').filter(|s| !s.is_empty()) {
            walk.push(segment);
            match fs::symlink_metadata(&walk) {
                Ok(meta) if is_link(&meta) => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "{} is a symbolic link inside a mount that does not follow links",
                            walk.display()
                        ),
                    ));
                }
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => break,
                Err(err) => return Err(err),
            }
        }
        Ok(path)
    }

    fn resolve(&self, relative: &str) -> PathBuf {
        // `relative` comes from a validated `VfsPath`: no `..`, no leading `/`, no backslashes or `:`,
        // so joining its segments cannot leave `root`.
        let mut path = self.root.clone();
        path.extend(relative.split('/').filter(|s| !s.is_empty()));
        path
    }
}

/// Whether `meta` (from `symlink_metadata`) is a symbolic link, or on Windows any reparse point
/// (junctions and mount points redirect like links).
fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        meta.file_type().is_symlink() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

/// Renames `from` over `to`, replacing an existing target. Atomic on Linux and Android. On Windows a
/// virus scanner, indexer or another reader can hold the target open for a moment, which fails the
/// replace with a sharing violation; those are retried a few times with a short backoff before the
/// error is returned (the write is never torn either way).
fn rename_replace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        const ATTEMPTS: u32 = 6;
        let mut attempt = 0;
        loop {
            match fs::rename(from, to) {
                Err(err) if attempt + 1 < ATTEMPTS && is_transient_sharing_error(&err) => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(5 << attempt));
                }
                other => return other,
            }
        }
    }
    #[cfg(not(windows))]
    {
        fs::rename(from, to)
    }
}

/// Whether `err` is a Windows access denied (5), sharing violation (32) or lock violation (33),
/// which clear once the other holder closes the file.
#[cfg(any(windows, test))]
fn is_transient_sharing_error(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(5 | 32 | 33))
}

/// Distinguishes concurrent temporary files of one process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Whether `name` is a `write_atomic` temporary: `<target>.<pid>-<counter>.tmp`.
fn is_write_temp(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".tmp") else {
        return false;
    };
    let Some((_, tag)) = stem.rsplit_once('.') else {
        return false;
    };
    let Some((pid, n)) = tag.split_once('-') else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    digits(pid) && digits(n)
}

/// `Ok(None)` when a directory entry's lookup failed because the entry no longer exists (removed
/// by another thread or process mid-listing); other errors pass through.
fn vanished_is_absent<T>(lookup: io::Result<T>) -> io::Result<Option<T>> {
    match lookup {
        Ok(value) => Ok(Some(value)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

impl VfsMount for DirMount {
    fn read(&self, relative: &str) -> io::Result<Vec<u8>> {
        fs::read(self.checked(relative)?)
    }

    fn open(&self, relative: &str) -> io::Result<Box<dyn VfsReader>> {
        let path = self.checked(relative)?;
        if path.is_dir() {
            return Err(io::Error::new(io::ErrorKind::IsADirectory, "is a directory"));
        }
        Ok(Box::new(fs::File::open(path)?))
    }

    fn exists(&self, relative: &str) -> bool {
        self.checked(relative).is_ok_and(|path| path.exists())
    }

    fn list_dir(&self, relative: &str) -> io::Result<Vec<VfsDirEntry>> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(self.checked(relative)?)? {
            let entry = entry?;
            // An in-flight `write_atomic` temp file is not a real entry (it vanishes at the rename).
            if is_write_temp(&entry.file_name().to_string_lossy()) {
                continue;
            }
            // An entry removed between `read_dir` and the type lookup (which may `stat`) is simply
            // not listed; it must not fail the whole listing.
            let Some(is_dir) = vanished_is_absent(entry.file_type().map(|kind| kind.is_dir()))? else {
                continue;
            };
            entries.push(VfsDirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir,
            });
        }
        entries.sort();
        Ok(entries)
    }

    fn write_atomic(&self, relative: &str, data: &[u8]) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::ReadOnlyFilesystem,
                "read-only mount",
            ));
        }
        let target = self.checked(relative)?;
        let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "no file name"));
        };
        fs::create_dir_all(dir)?;
        let mut temp_name = name.to_os_string();
        temp_name.push(format!(
            ".{}-{}.tmp",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let temp = dir.join(temp_name);
        let result = (|| {
            // `create_new`: never opens an existing file or follows a link planted on the name.
            let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&temp)?;
            file.write_all(data)?;
            file.sync_all()?;
            drop(file);
            // `rename_replace` replaces an existing target atomically on Linux/Android and Windows.
            rename_replace(&temp, &target)?;
            // Make the new directory entry durable too (Unix; Windows cannot open a directory
            // as a file here, and its rename is already write-through for NTFS metadata).
            #[cfg(unix)]
            {
                if let Ok(parent) = fs::File::open(dir) {
                    let _ = parent.sync_all();
                }
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    fn host_path(&self, relative: &str) -> Option<PathBuf> {
        self.checked(relative).ok()
    }

    fn describe(&self) -> String {
        let mode = if self.writable { "rw" } else { "ro" };
        format!("dir ({mode}) {}", self.root.display())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A unique temporary directory, removed on drop.
    pub(crate) struct TempDir(PathBuf);

    impl TempDir {
        pub(crate) fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "regia-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn sharing_violations_are_transient_and_other_errors_are_not() {
        for code in [5, 32, 33] {
            assert!(
                is_transient_sharing_error(&io::Error::from_raw_os_error(code)),
                "{code}"
            );
        }
        assert!(!is_transient_sharing_error(&io::Error::from_raw_os_error(2)));
        assert!(!is_transient_sharing_error(&io::Error::other("x")));
    }

    #[test]
    fn rename_replace_overwrites_an_existing_file() {
        let tmp = TempDir::new("dir-replace");
        let (from, to) = (tmp.path().join("from"), tmp.path().join("to"));
        fs::write(&from, b"new").expect("write from");
        fs::write(&to, b"old").expect("write to");
        rename_replace(&from, &to).expect("replace");
        assert_eq!(fs::read(&to).expect("read"), b"new");
        assert!(!from.exists());
    }

    #[test]
    fn resolve_stays_under_root() {
        let m = DirMount::read_only("/r");
        assert_eq!(m.resolve(""), PathBuf::from("/r"));
        assert_eq!(m.resolve("a/b.txt"), Path::new("/r").join("a").join("b.txt"));
    }

    #[test]
    fn an_entry_that_vanished_mid_listing_is_skipped_not_an_error() {
        let gone: io::Result<bool> = Err(io::Error::from(io::ErrorKind::NotFound));
        assert!(matches!(vanished_is_absent(gone), Ok(None)));
        assert!(matches!(vanished_is_absent(Ok(true)), Ok(Some(true))));
        let denied: io::Result<bool> = Err(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(
            vanished_is_absent(denied).is_err(),
            "other errors still fail the listing"
        );
    }

    #[test]
    fn list_dir_hides_write_atomic_temp_files() {
        assert!(is_write_temp("slot1.sav.1234-7.tmp"));
        assert!(!is_write_temp("notes.tmp"));
        assert!(!is_write_temp("a.b-c.tmp"));
        assert!(!is_write_temp("slot1.sav"));
        let tmp = TempDir::new("dir-temp-hidden");
        let m = DirMount::writable(tmp.path());
        m.write_atomic("slot1.sav", b"x").expect("write");
        fs::write(tmp.path().join("slot1.sav.1234-7.tmp"), b"partial").expect("temp");
        let names: Vec<_> = m
            .list_dir("")
            .expect("list")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["slot1.sav"]);
    }

    /// Audit v0.2.12 (security): a link planted inside a writable mount moved reads and writes
    /// outside its root (`resolve` is lexical only).
    #[cfg(unix)]
    #[test]
    fn a_writable_mount_does_not_follow_a_link_out_of_its_root() {
        let tmp = TempDir::new("dir-link-escape");
        let root = tmp.path().join("user");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(outside.join("secret.txt"), b"secret").expect("secret");
        std::os::unix::fs::symlink(&outside, root.join("saves")).expect("dir link");
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("slot.sav")).expect("file link");
        std::os::unix::fs::symlink(outside.join("planted.txt"), root.join("dangling.sav")).expect("dangling");
        let m = DirMount::writable(&root);
        assert!(!m.follows_symlinks());
        for path in ["saves/secret.txt", "slot.sav"] {
            let err = m.read(path).expect_err(path);
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{path}");
            assert!(m.open(path).is_err(), "{path}");
            assert!(!m.exists(path), "{path}");
            assert!(m.host_path(path).is_none(), "{path}");
        }
        assert!(m.list_dir("saves").is_err());
        for path in ["saves/new.sav", "saves/deep/new.sav", "slot.sav", "dangling.sav"] {
            let err = m.write_atomic(path, b"x").expect_err(path);
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{path}");
        }
        assert_eq!(fs::read(outside.join("secret.txt")).expect("read"), b"secret");
        assert!(!outside.join("new.sav").exists());
        assert!(!outside.join("deep").exists());
        assert!(!outside.join("planted.txt").exists());
        // Real files and folders still work, and the root itself may be a link.
        m.write_atomic("real/ok.sav", b"ok").expect("real write");
        assert_eq!(m.read("real/ok.sav").expect("read"), b"ok");
        let linked_root = tmp.path().join("root-link");
        std::os::unix::fs::symlink(&root, &linked_root).expect("root link");
        assert_eq!(
            DirMount::writable(&linked_root)
                .read("real/ok.sav")
                .expect("through a linked root"),
            b"ok"
        );
        // A read-only view of a folder others write refuses links too; packaged content follows.
        let logs = DirMount::read_only(&root).without_symlinks();
        assert!(logs.read("saves/secret.txt").is_err());
        assert_eq!(
            DirMount::read_only(&root)
                .read("saves/secret.txt")
                .expect("packaged content follows"),
            b"secret"
        );
    }

    #[test]
    fn read_only_mount_refuses_writes() {
        let tmp = TempDir::new("dir-ro");
        let m = DirMount::read_only(tmp.path());
        let err = m.write_atomic("a.txt", b"x").expect_err("read-only");
        assert_eq!(err.kind(), io::ErrorKind::ReadOnlyFilesystem);
        assert!(!tmp.path().join("a.txt").exists());
    }

    #[test]
    fn writable_mount_creates_root_lazily() {
        let tmp = TempDir::new("dir-lazy");
        let root = tmp.path().join("not/yet");
        let m = DirMount::writable(&root);
        assert!(!m.exists(""));
        m.write_atomic("deep/file.bin", &[1, 2, 3]).expect("write");
        assert_eq!(m.read("deep/file.bin").expect("read"), [1, 2, 3]);
        assert!(m.open("deep").is_err(), "directories cannot be streamed");
    }

    #[test]
    fn concurrent_atomic_writes_leave_one_whole_file() {
        let tmp = TempDir::new("dir-race");
        let m = std::sync::Arc::new(DirMount::writable(tmp.path()));
        let threads: Vec<_> = (0..8u8)
            .map(|i| {
                let m = m.clone();
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        let result = m.write_atomic("state.bin", &[i; 4096]);
                        // Windows may refuse a replace while another thread's rename holds the target
                        // open (sharing violation); that is a clean failure, never a torn file.
                        #[cfg(not(windows))]
                        result.expect("write");
                        #[cfg(windows)]
                        let _ = result;
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("writer thread");
        }
        let data = m.read("state.bin").expect("read");
        assert_eq!(data.len(), 4096);
        assert!(data.iter().all(|b| *b == data[0]), "torn write");
        let names: Vec<_> = m
            .list_dir("")
            .expect("list")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["state.bin"]);
    }
}
