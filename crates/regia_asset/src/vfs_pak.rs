// SPDX-License-Identifier: MPL-2.0
//! `.fpak` archives as read-only [`Vfs`](regia_platform::Vfs) mounts (`docs/specs/11-platform-and-input.md`
//! § 4, ADR-7232).
//!
//! [`PakVfsMount`] indexes a pak's manifest: every manifest path that is a valid VFS path is a
//! file, every proper prefix of one is a directory. A cook writes project-relative paths
//! (`assets/textures/rock.ktx2`); the leading `assets/` segment is dropped, so a game's pak mounted
//! at `assets://` serves `assets://textures/rock.ktx2`. Other paths are served as written. Reading a file returns the entry's stored bytes,
//! decompressed: a cooked file (`CookedHeader` + payload) for cooked content, the file itself for a
//! raw one. Manifest paths that are not valid VFS paths (a `#` sub-asset path holding `:`, say) are
//! left out with a debug log; they are still reachable through the asset server by id.
//!
//! `regia_platform` cannot read paks (this crate depends on it), so [`install_vfs_pak_mounts`]
//! installs [`PakVfsMount::open_mount`] as the platform's pak opener; [`AssetPlugin`](crate::AssetPlugin)
//! calls it. After that `Vfs::mount_pak` works in the whole process.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_platform::{MountPoint, VfsDirEntry, VfsMount, VfsPath, VfsReader};

use crate::id::AssetId;
use crate::pak::PakReader;

/// A `.fpak` archive mounted read-only in the VFS. Every read is a copy of the entry.
pub struct PakVfsMount {
    reader: Arc<PakReader>,
    files: BTreeMap<String, AssetId>,
    dirs: BTreeSet<String>,
    path: PathBuf,
}

impl std::fmt::Debug for PakVfsMount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PakVfsMount")
            .field("path", &self.path)
            .field("files", &self.files.len())
            .finish()
    }
}

impl PakVfsMount {
    /// Opens `path` and indexes its manifest.
    ///
    /// # Errors
    ///
    /// The file does not open or is not a valid pak (`InvalidData` for a bad header or table of
    /// contents).
    pub fn open(path: &Path) -> io::Result<Self> {
        let reader = PakReader::open(path).map_err(|e| match e {
            crate::pak::PakError::Io(io) => io,
            other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
        })?;
        // Every read is a copy, so the mapping buys nothing; reading through the handle keeps a
        // pak rewritten in place from faulting a read (as `mods://` does, ADR-5869).
        reader.set_zero_copy(false);
        let mut files = BTreeMap::new();
        let mut dirs = BTreeSet::new();
        for entry in reader.manifest() {
            let manifest_path = entry.path.strip_prefix("assets/").unwrap_or(&entry.path);
            let relative = match VfsPath::new(MountPoint::Assets, manifest_path) {
                Ok(p) if !p.is_root() && reader.contains(entry.id) => p.relative().to_owned(),
                _ => {
                    tracing::debug!(target: "regia_asset", path = %entry.path, pak = %path.display(),
                        "vfs pak: manifest path is not a VFS file path; left out");
                    continue;
                }
            };
            let mut prefix = relative.as_str();
            while let Some((parent, _)) = prefix.rsplit_once('/') {
                dirs.insert(parent.to_owned());
                prefix = parent;
            }
            // The first entry for a path wins (`assets/x` and `x` in one pak name one file).
            files.entry(relative).or_insert(entry.id);
        }
        Ok(Self {
            reader: Arc::new(reader),
            files,
            dirs,
            path: path.to_path_buf(),
        })
    }

    /// [`Self::open`] as the platform's pak opener ([`regia_platform::PakMountOpener`]).
    ///
    /// # Errors
    ///
    /// As [`Self::open`].
    pub fn open_mount(path: &Path) -> io::Result<Arc<dyn VfsMount>> {
        Ok(Arc::new(Self::open(path)?))
    }

    /// How many files the mount serves.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// The archive's host path.
    #[must_use]
    pub fn pak_path(&self) -> &Path {
        &self.path
    }

    fn id_of(&self, relative: &str) -> io::Result<AssetId> {
        self.files
            .get(relative)
            .copied()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{relative}: not in the pak")))
    }
}

impl VfsMount for PakVfsMount {
    fn read(&self, relative: &str) -> io::Result<Vec<u8>> {
        let id = self.id_of(relative)?;
        self.reader
            .read_decompressed(id)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{relative}: {e}")))
    }

    fn open(&self, relative: &str) -> io::Result<Box<dyn VfsReader>> {
        Ok(Box::new(io::Cursor::new(self.read(relative)?)))
    }

    fn exists(&self, relative: &str) -> bool {
        relative.is_empty() || self.files.contains_key(relative) || self.dirs.contains(relative)
    }

    fn list_dir(&self, relative: &str) -> io::Result<Vec<VfsDirEntry>> {
        if !relative.is_empty() && !self.dirs.contains(relative) {
            let kind = if self.files.contains_key(relative) {
                io::ErrorKind::NotADirectory
            } else {
                io::ErrorKind::NotFound
            };
            return Err(io::Error::new(
                kind,
                format!("{relative}: not a directory of the pak"),
            ));
        }
        let prefix = if relative.is_empty() {
            String::new()
        } else {
            format!("{relative}/")
        };
        let child = |path: &str| -> Option<String> {
            let rest = path.strip_prefix(prefix.as_str())?;
            (!rest.is_empty() && !rest.contains('/')).then(|| rest.to_owned())
        };
        let mut out: Vec<VfsDirEntry> = self
            .dirs
            .iter()
            .filter_map(|d| child(d))
            .map(|name| VfsDirEntry { name, is_dir: true })
            .chain(
                self.files
                    .keys()
                    .filter_map(|f| child(f))
                    .map(|name| VfsDirEntry { name, is_dir: false }),
            )
            .collect();
        out.sort();
        Ok(out)
    }

    fn describe(&self) -> String {
        format!("pak {} ({} files)", self.path.display(), self.files.len())
    }
}

/// Installs [`PakVfsMount::open_mount`] as the process-wide opener of `Vfs::mount_pak`. Returns
/// false when an opener was already installed (this one or another). Idempotent in effect.
pub fn install_vfs_pak_mounts() -> bool {
    regia_platform::install_pak_mount_opener(PakVfsMount::open_mount)
}
