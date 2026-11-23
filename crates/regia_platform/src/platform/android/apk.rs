// SPDX-License-Identifier: MPL-2.0
//! [`ApkAssetMount`]: read-only files packaged in the APK's `assets/` folder, through the NDK
//! `AAssetManager`.

use std::ffi::CString;
use std::io::{self, Read, Seek, SeekFrom};

use ndk::asset::{Asset, AssetManager};

use crate::vfs::{VfsDirEntry, VfsMount, VfsReader, apk_asset_name};

/// A read-only mount of an APK assets subdirectory (`""` for the assets root).
///
/// Limitation of the NDK API: [`VfsMount::list_dir`] lists only the *files* of a directory, not its
/// subdirectories. Cooked `.fpak` archives (M4) carry their own index, so the engine does not rely
/// on directory listings of APK assets.
#[derive(Debug)]
pub struct ApkAssetMount {
    manager: AssetManager,
    subdir: String,
}

impl ApkAssetMount {
    /// Mounts the APK assets under `subdir` (`/` separated; empty for the root).
    pub fn new(manager: AssetManager, subdir: impl Into<String>) -> Self {
        Self {
            manager,
            subdir: subdir.into(),
        }
    }

    fn name(&self, relative: &str) -> io::Result<CString> {
        CString::new(apk_asset_name(&self.subdir, relative))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in asset path"))
    }

    fn open_asset(&self, relative: &str) -> io::Result<Asset> {
        self.manager
            .open(&self.name(relative)?)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such APK asset"))
    }
}

/// An open APK asset that can move to another thread.
struct SendAsset(Asset);

// SAFETY: an `AAsset` must not be used from two threads at the same time, but it is not bound to the
// thread that opened it. `SendAsset` owns its asset exclusively and is not `Sync`, so moving it to
// another thread can never cause concurrent use.
unsafe impl Send for SendAsset {}

impl Read for SendAsset {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Seek for SendAsset {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.seek(pos)
    }
}

impl VfsMount for ApkAssetMount {
    fn read(&self, relative: &str) -> io::Result<Vec<u8>> {
        let mut asset = self.open_asset(relative)?;
        let mut data = Vec::with_capacity(asset.length());
        asset.read_to_end(&mut data)?;
        Ok(data)
    }

    fn open(&self, relative: &str) -> io::Result<Box<dyn VfsReader>> {
        Ok(Box::new(SendAsset(self.open_asset(relative)?)))
    }

    fn exists(&self, relative: &str) -> bool {
        if relative.is_empty() {
            return true;
        }
        if self.open_asset(relative).is_ok() {
            return true;
        }
        // A directory "exists" when it has at least one file (the NDK cannot see empty or
        // subdirectory-only directories).
        self.name(relative)
            .ok()
            .and_then(|name| self.manager.open_dir(&name))
            .is_some_and(|mut dir| dir.next().is_some())
    }

    fn list_dir(&self, relative: &str) -> io::Result<Vec<VfsDirEntry>> {
        let dir = self
            .manager
            .open_dir(&self.name(relative)?)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such APK asset directory"))?;
        let mut entries: Vec<_> = dir
            .map(|name| VfsDirEntry {
                name: name.to_string_lossy().into_owned(),
                is_dir: false,
            })
            .collect();
        entries.sort();
        Ok(entries)
    }

    fn describe(&self) -> String {
        format!("apk assets/{}", self.subdir)
    }
}
