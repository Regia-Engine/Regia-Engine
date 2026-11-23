// SPDX-License-Identifier: MPL-2.0
//! Virtual file system (`docs/specs/11-platform-and-input.md` § 4; M1 subset per ADR-012 § 3).
//!
//! Every engine file access goes through the [`Vfs`] resource with [`VfsPath`]s such as
//! `assets://textures/rock.ktx2` or `user://saves/slot1.sav`. Each [`MountPoint`] is backed by one
//! [`VfsMount`]: a host directory ([`DirMount`]) or, on Android, the APK's assets
//! (`ApkAssetMount`).
//!
//! Default mounts (see [`Vfs::with_default_mounts`]):
//!
//! | Mount | Desktop (M1, development) | Android |
//! |---|---|---|
//! | `assets://` | `$REGIA_ASSETS`, else `<exe dir>/assets`, else `$CARGO_MANIFEST_DIR/assets` | APK assets root |
//! | `config://` | `<assets>/config` | APK `assets/config` |
//! | `user://` | `dirs::data_dir()/RegiaEngine/<exe stem>` | `internal_data_path()` |
//! | `cache://` | `dirs::cache_dir()/RegiaEngine/<exe stem>` | `Context.getCacheDir()` |
//! | `logs://` | `user://logs` | `user://logs` |
//!
//! `user://`, `cache://` and `logs://` never follow a symbolic link below their root
//! ([`DirMount::without_symlinks`]); `assets://` and `config://` do.
//!
//! Cooked content is read by `regia_asset::AssetServer`. [`Vfs::mount_pak`] mounts a `.fpak`
//! archive read-only at a mount point: each manifest path of the pak is a file, and reading it
//! returns that entry's stored bytes (ADR-7232). This crate cannot read paks itself (the reader
//! lives in `regia_asset`, which depends on this crate), so `regia_asset` installs the opener
//! ([`install_pak_mount_opener`]; its `AssetPlugin` does that); without one `mount_pak` reports
//! [`VfsError::Unsupported`]. [`Vfs::read`] runs on the caller: it finishes `read_sync` before it
//! returns, so the future is already complete (ADR-5070).

mod dir;
mod path;
pub mod stats;

use std::future::Future;
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use regia_app::prelude::*;

pub use dir::DirMount;
pub use path::{MountPoint, VfsPath, VfsPathError, is_reserved_device_name};
pub use stats::VfsStats;

/// A seekable byte stream returned by [`Vfs::open_stream`].
pub trait VfsReader: Read + Seek + Send {}
impl<T: Read + Seek + Send> VfsReader for T {}

/// One entry of [`Vfs::list_dir`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct VfsDirEntry {
    /// File or directory name (one segment).
    pub name: String,
    /// True for directories.
    pub is_dir: bool,
}

/// A backend for one [`MountPoint`]. Paths passed in are already validated relative paths (`/`
/// separated, no `..`, empty for the mount root).
pub trait VfsMount: Send + Sync + 'static {
    /// Reads a whole file.
    fn read(&self, relative: &str) -> io::Result<Vec<u8>>;
    /// Opens a file for streaming reads with seeking.
    fn open(&self, relative: &str) -> io::Result<Box<dyn VfsReader>>;
    /// Whether a file or directory exists.
    fn exists(&self, relative: &str) -> bool;
    /// Lists a directory, sorted by name.
    fn list_dir(&self, relative: &str) -> io::Result<Vec<VfsDirEntry>>;
    /// Replaces a file atomically (write to a temporary file, then rename). Read-only backends
    /// return an error of kind [`io::ErrorKind::ReadOnlyFilesystem`].
    fn write_atomic(&self, relative: &str, data: &[u8]) -> io::Result<()> {
        let _ = (relative, data);
        Err(io::Error::new(
            io::ErrorKind::ReadOnlyFilesystem,
            "read-only mount",
        ))
    }
    /// The host path of `relative`, for backends that are plain directories.
    fn host_path(&self, relative: &str) -> Option<PathBuf> {
        let _ = relative;
        None
    }
    /// Short description for logs, e.g. `dir /home/me/game/assets`.
    fn describe(&self) -> String;
}

/// VFS failures. Every variant carries the VFS path it is about.
#[derive(Debug, thiserror::Error)]
pub enum VfsError {
    /// The string is not a valid [`VfsPath`].
    #[error("invalid VFS path {path:?}: {reason}")]
    InvalidPath {
        /// The rejected string.
        path: String,
        /// Why.
        reason: VfsPathError,
    },
    /// Nothing is mounted at the path's mount point.
    #[error("{path}: nothing mounted at {}://", path.mount())]
    NotMounted {
        /// The path.
        path: VfsPath,
    },
    /// The file or directory does not exist.
    #[error("{path}: not found")]
    NotFound {
        /// The path.
        path: VfsPath,
    },
    /// Writing outside `user://` and `cache://`, or to a read-only backend.
    #[error("{path}: read-only (only user:// and cache:// are writable)")]
    ReadOnly {
        /// The path.
        path: VfsPath,
    },
    /// An IO error from the backend.
    #[error("{path}: {source}")]
    Io {
        /// The path.
        path: VfsPath,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A feature that is not available yet (or not on this platform).
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    /// [`Vfs::mount_pak`] could not open the archive (missing file, not a pak, corrupt table of
    /// contents).
    #[error("pak {}: {source}", pak.display())]
    PakOpen {
        /// The archive's host path.
        pak: PathBuf,
        /// Why it did not open.
        #[source]
        source: io::Error,
    },
}

/// Opens a `.fpak` archive as a read-only [`VfsMount`]. `regia_asset` provides it
/// (`regia_asset::install_vfs_pak_mounts`), since the pak reader lives there.
pub type PakMountOpener = fn(&Path) -> io::Result<Arc<dyn VfsMount>>;

static PAK_MOUNT_OPENER: OnceLock<PakMountOpener> = OnceLock::new();

/// Installs the process-wide opener [`Vfs::mount_pak`] uses. The first call wins; returns whether
/// this call installed `opener` (false when one was already installed).
pub fn install_pak_mount_opener(opener: PakMountOpener) -> bool {
    PAK_MOUNT_OPENER.set(opener).is_ok()
}

/// Whether a pak opener is installed, so [`Vfs::mount_pak`] can mount archives.
pub fn pak_mounts_available() -> bool {
    PAK_MOUNT_OPENER.get().is_some()
}

impl VfsError {
    fn from_io(path: &VfsPath, source: io::Error) -> Self {
        match source.kind() {
            io::ErrorKind::NotFound => VfsError::NotFound { path: path.clone() },
            io::ErrorKind::ReadOnlyFilesystem => VfsError::ReadOnly { path: path.clone() },
            _ => VfsError::Io {
                path: path.clone(),
                source,
            },
        }
    }
}

/// The virtual file system resource. Cloning is cheap (mounts are shared), so IO tasks can take a
/// copy.
#[derive(Resource, Clone, Default)]
pub struct Vfs {
    mounts: [Option<Arc<dyn VfsMount>>; MountPoint::ALL.len()],
}

impl std::fmt::Debug for Vfs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut map = f.debug_map();
        for point in MountPoint::ALL {
            if let Some(m) = &self.mounts[point.index()] {
                map.entry(&point.as_str(), &m.describe());
            }
        }
        map.finish()
    }
}

impl Vfs {
    /// A VFS with nothing mounted.
    pub fn new() -> Self {
        Self::default()
    }

    /// The platform's default mounts (table in the module docs). On Android this needs the
    /// `AndroidAppHandle` resource in `world`; without it nothing is mounted (and a warning is
    /// logged).
    pub fn with_default_mounts(world: &World) -> Self {
        crate::platform::default_vfs(world)
    }

    /// Mounts `backend` at `point`, replacing what was there.
    ///
    /// ```
    /// use regia_platform::{DirMount, MountPoint, Vfs};
    ///
    /// let mut vfs = Vfs::new();
    /// assert!(!vfs.is_mounted(MountPoint::Cache));
    /// vfs.mount(MountPoint::Cache, DirMount::writable(std::env::temp_dir()));
    /// assert!(vfs.is_mounted(MountPoint::Cache));
    /// vfs.unmount(MountPoint::Cache);
    /// assert!(!vfs.is_mounted(MountPoint::Cache));
    /// ```
    pub fn mount(&mut self, point: MountPoint, backend: impl VfsMount) {
        tracing::debug!(target: "regia_platform", "vfs: {point}:// -> {}", backend.describe());
        self.mounts[point.index()] = Some(Arc::new(backend));
    }

    /// Mounts a `.fpak` archive read-only at `point`, replacing what was there: every manifest path
    /// of the pak is a file of the mount (`assets://textures/rock.ktx2` for the manifest path
    /// `textures/rock.ktx2`), and reading it returns the entry's stored bytes (a cooked file for
    /// cooked content, the file itself for a raw one), decompressed. Directories are the path
    /// prefixes. Writes are refused.
    ///
    /// # Errors
    ///
    /// [`VfsError::Unsupported`] when no pak opener is installed ([`install_pak_mount_opener`];
    /// `regia_asset`'s `AssetPlugin` installs it), [`VfsError::PakOpen`] when the archive does not
    /// open. The previous mount stays on error.
    pub fn mount_pak(&mut self, point: MountPoint, pak: impl AsRef<Path>) -> Result<(), VfsError> {
        let pak = pak.as_ref();
        let Some(open) = PAK_MOUNT_OPENER.get() else {
            return Err(VfsError::Unsupported(
                "no pak reader is installed (regia_asset::install_vfs_pak_mounts or its AssetPlugin installs it)",
            ));
        };
        let backend = open(pak).map_err(|source| VfsError::PakOpen {
            pak: pak.to_path_buf(),
            source,
        })?;
        tracing::debug!(target: "regia_platform", "vfs: {point}:// -> {}", backend.describe());
        self.mounts[point.index()] = Some(backend);
        Ok(())
    }

    /// Removes the mount at `point`.
    pub fn unmount(&mut self, point: MountPoint) {
        self.mounts[point.index()] = None;
    }

    /// Whether something is mounted at `point`.
    pub fn is_mounted(&self, point: MountPoint) -> bool {
        self.mounts[point.index()].is_some()
    }

    /// The backend description of `point`, for diagnostics.
    pub fn describe_mount(&self, point: MountPoint) -> Option<String> {
        self.mounts[point.index()].as_ref().map(|m| m.describe())
    }

    fn backend(&self, path: &VfsPath) -> Result<&Arc<dyn VfsMount>, VfsError> {
        self.mounts[path.mount().index()]
            .as_ref()
            .ok_or_else(|| VfsError::NotMounted { path: path.clone() })
    }

    /// Reads a whole file.
    pub fn read_sync(&self, path: &VfsPath) -> Result<Arc<[u8]>, VfsError> {
        let _span = tracing::trace_span!("vfs_read", path = %path).entered();
        let read = self
            .backend(path)
            .and_then(|b| b.read(path.relative()).map_err(|e| VfsError::from_io(path, e)));
        match &read {
            Ok(bytes) => stats::note_open(bytes.len()),
            Err(_) => stats::note_failed_open(),
        }
        read.map(Arc::from)
    }

    /// Reads a whole file. The read runs on the caller, before this returns, so the future is
    /// already complete.
    pub fn read(&self, path: &VfsPath) -> impl Future<Output = Result<Arc<[u8]>, VfsError>> + Send + 'static {
        std::future::ready(self.read_sync(path))
    }

    /// Opens a file as a seekable stream.
    pub fn open_stream(&self, path: &VfsPath) -> Result<Box<dyn VfsReader>, VfsError> {
        let opened = self
            .backend(path)
            .and_then(|b| b.open(path.relative()).map_err(|e| VfsError::from_io(path, e)));
        match opened {
            Ok(reader) => {
                stats::note_open(0);
                Ok(Box::new(stats::CountingReader(reader)))
            }
            Err(e) => {
                stats::note_failed_open();
                Err(e)
            }
        }
    }

    /// Replaces a file atomically: the data goes to a temporary file next to it, which is then
    /// renamed over the target, so readers see the old or the new content, never a mix. Missing
    /// parent directories are created. Only `user://` and `cache://` are writable.
    pub fn write_atomic(&self, path: &VfsPath, data: &[u8]) -> Result<(), VfsError> {
        if !path.mount().is_writable() {
            return Err(VfsError::ReadOnly { path: path.clone() });
        }
        if path.is_root() {
            return Err(VfsError::Io {
                path: path.clone(),
                source: io::Error::new(io::ErrorKind::InvalidInput, "cannot write to a mount root"),
            });
        }
        let _span = tracing::trace_span!("vfs_write", path = %path).entered();
        self.backend(path)?
            .write_atomic(path.relative(), data)
            .map_err(|e| VfsError::from_io(path, e))?;
        stats::note_write(data.len());
        Ok(())
    }

    /// Whether a file or directory exists (false when nothing is mounted).
    pub fn exists(&self, path: &VfsPath) -> bool {
        self.backend(path)
            .map(|b| b.exists(path.relative()))
            .unwrap_or(false)
    }

    /// Lists a directory, sorted by name.
    pub fn list_dir(&self, path: &VfsPath) -> Result<Vec<VfsDirEntry>, VfsError> {
        self.backend(path)?
            .list_dir(path.relative())
            .map_err(|e| VfsError::from_io(path, e))
    }

    /// The host file system path behind `path`, when its mount is a plain directory.
    pub fn host_path(&self, path: &VfsPath) -> Option<PathBuf> {
        self.backend(path).ok()?.host_path(path.relative())
    }
}

/// Picks the desktop development `assets://` root: `REGIA_ASSETS` if set, else `<exe dir>/assets`
/// if it exists, else `<CARGO_MANIFEST_DIR>/assets` if it exists (`cargo run` sets that variable),
/// else `<exe dir>/assets`.
#[cfg_attr(target_os = "android", allow(dead_code))]
pub(crate) fn resolve_assets_root(
    env_assets: Option<PathBuf>,
    exe_dir: Option<PathBuf>,
    manifest_dir: Option<PathBuf>,
    is_dir: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if let Some(dir) = env_assets.filter(|d| !d.as_os_str().is_empty()) {
        return Some(dir);
    }
    let beside_exe = exe_dir.map(|d| d.join("assets"));
    if let Some(dir) = beside_exe.as_ref().filter(|d| is_dir(d)) {
        return Some(dir.clone());
    }
    if let Some(dir) = manifest_dir.map(|d| d.join("assets")).filter(|d| is_dir(d)) {
        return Some(dir);
    }
    beside_exe
}

/// The asset-manager name of `relative` inside the APK subdirectory `subdir` (both `/` separated,
/// either may be empty).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn apk_asset_name(subdir: &str, relative: &str) -> String {
    let subdir = subdir.trim_matches('/');
    match (subdir.is_empty(), relative.is_empty()) {
        (true, _) => relative.to_owned(),
        (false, true) => subdir.to_owned(),
        (false, false) => format!("{subdir}/{relative}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::SeekFrom;

    use dir::tests::TempDir;

    fn path(s: &str) -> VfsPath {
        VfsPath::parse(s).expect("valid test path")
    }

    fn test_vfs(tmp: &TempDir) -> Vfs {
        let mut vfs = Vfs::new();
        let assets = tmp.path().join("assets");
        std::fs::create_dir_all(assets.join("config")).expect("mkdir");
        std::fs::write(assets.join("hello.txt"), b"hello vfs").expect("write");
        std::fs::write(assets.join("config/game.toml"), b"x = 1").expect("write");
        vfs.mount(MountPoint::Assets, DirMount::read_only(&assets));
        vfs.mount(MountPoint::Config, DirMount::read_only(assets.join("config")));
        vfs.mount(MountPoint::User, DirMount::writable(tmp.path().join("user")));
        vfs.mount(MountPoint::Cache, DirMount::writable(tmp.path().join("cache")));
        vfs.mount(
            MountPoint::Logs,
            DirMount::read_only(tmp.path().join("user/logs")),
        );
        vfs
    }

    #[test]
    fn read_sync_async_and_stream() {
        let tmp = TempDir::new("vfs-read");
        let vfs = test_vfs(&tmp);
        let hello = path("assets://hello.txt");
        assert_eq!(&*vfs.read_sync(&hello).expect("read"), b"hello vfs");
        assert_eq!(
            &*vfs.read_sync(&path("config://game.toml")).expect("read"),
            b"x = 1"
        );

        let fut = vfs.read(&hello);
        let bytes = poll_ready(fut).expect("read");
        assert_eq!(&*bytes, b"hello vfs");

        let mut stream = vfs.open_stream(&hello).expect("open");
        stream.seek(SeekFrom::Start(6)).expect("seek");
        let mut rest = String::new();
        stream.read_to_string(&mut rest).expect("read");
        assert_eq!(rest, "vfs");
    }

    #[test]
    fn missing_and_unmounted() {
        let tmp = TempDir::new("vfs-missing");
        let vfs = test_vfs(&tmp);
        let missing = path("assets://nope.bin");
        assert!(matches!(vfs.read_sync(&missing), Err(VfsError::NotFound { path }) if path == missing));
        assert!(!vfs.exists(&missing));
        let mut empty = Vfs::new();
        assert!(matches!(
            empty.read_sync(&missing),
            Err(VfsError::NotMounted { .. })
        ));
        assert!(!empty.exists(&missing));
        // No pak opener is installed in this crate's tests (regia_asset installs it).
        assert!(matches!(
            empty.mount_pak(MountPoint::Assets, "content/base.fpak"),
            Err(VfsError::Unsupported(msg)) if msg.contains("pak reader")
        ));
        assert!(!empty.is_mounted(MountPoint::Assets));
    }

    #[test]
    fn write_atomic_only_on_user_and_cache() {
        let tmp = TempDir::new("vfs-write");
        let vfs = test_vfs(&tmp);
        let save = path("user://saves/slot1.sav");
        vfs.write_atomic(&save, b"one").expect("write user");
        vfs.write_atomic(&save, b"two").expect("overwrite user");
        assert_eq!(&*vfs.read_sync(&save).expect("read back"), b"two");
        assert!(vfs.exists(&path("user://saves")));
        vfs.write_atomic(&path("cache://shaders/a.bin"), b"c")
            .expect("write cache");

        for ro in ["assets://hello.txt", "config://game.toml", "logs://session.log"] {
            let p = path(ro);
            assert!(
                matches!(vfs.write_atomic(&p, b"x"), Err(VfsError::ReadOnly { .. })),
                "{ro} must be read-only"
            );
        }
        assert_eq!(
            &*vfs.read_sync(&path("assets://hello.txt")).expect("read"),
            b"hello vfs"
        );
        assert!(vfs.write_atomic(&path("user://"), b"x").is_err());
        // No temporary files left behind.
        let names: Vec<_> = vfs
            .list_dir(&path("user://saves"))
            .expect("list")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["slot1.sav"]);
    }

    #[test]
    fn list_dir_sorted_with_kinds() {
        let tmp = TempDir::new("vfs-list");
        let vfs = test_vfs(&tmp);
        let entries = vfs.list_dir(&path("assets://")).expect("list");
        assert_eq!(
            entries,
            [
                VfsDirEntry {
                    name: "config".into(),
                    is_dir: true
                },
                VfsDirEntry {
                    name: "hello.txt".into(),
                    is_dir: false
                }
            ]
        );
        assert!(matches!(
            vfs.list_dir(&path("assets://nope")),
            Err(VfsError::NotFound { .. })
        ));
        assert_eq!(
            vfs.host_path(&path("assets://hello.txt")),
            Some(tmp.path().join("assets").join("hello.txt"))
        );
    }

    #[test]
    fn assets_root_resolution() {
        let exe = PathBuf::from("/game/bin");
        let manifest = PathBuf::from("/src/game");
        let none = |_: &Path| false;
        let all = |_: &Path| true;
        let only_manifest = |p: &Path| p.starts_with("/src");
        assert_eq!(
            resolve_assets_root(Some("/x".into()), Some(exe.clone()), None, all),
            Some(PathBuf::from("/x"))
        );
        assert_eq!(
            resolve_assets_root(Some("".into()), Some(exe.clone()), None, all),
            Some(exe.join("assets"))
        );
        assert_eq!(
            resolve_assets_root(None, Some(exe.clone()), Some(manifest.clone()), only_manifest),
            Some(manifest.join("assets"))
        );
        assert_eq!(
            resolve_assets_root(None, Some(exe.clone()), Some(manifest), none),
            Some(exe.join("assets"))
        );
        assert_eq!(resolve_assets_root(None, None, None, none), None);
    }

    #[test]
    fn apk_names() {
        assert_eq!(apk_asset_name("", "a/b.png"), "a/b.png");
        assert_eq!(apk_asset_name("", ""), "");
        assert_eq!(apk_asset_name("config", ""), "config");
        assert_eq!(apk_asset_name("/config/", "game.toml"), "config/game.toml");
    }

    /// Polls a future once; M1's `read` future is always ready.
    fn poll_ready<F: Future>(fut: F) -> F::Output {
        use std::task::{Context, Poll, Waker};
        let mut fut = std::pin::pin!(fut);
        match fut.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(out) => out,
            Poll::Pending => panic!("M1 Vfs::read must complete immediately"),
        }
    }
}
