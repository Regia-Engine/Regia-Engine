// SPDX-License-Identifier: MPL-2.0
//! The editor's file-dialog seam. `rfd` is a desktop-only dependency of this crate: rfd 0.17 has
//! no Android implementation of its dialog traits, and the Android workspace check compiles the
//! editor, so Android builds get a no-op `FileDialog` whose pickers answer `None` (ADR-10356).
//! Editor code imports `crate::file_dialog::FileDialog`, never `rfd` directly.

#[cfg(not(target_os = "android"))]
pub(crate) use rfd::FileDialog;

#[cfg(target_os = "android")]
use std::path::{Path, PathBuf};

#[cfg(target_os = "android")]
#[derive(Clone, Debug, Default)]
pub(crate) struct FileDialog;

#[cfg(target_os = "android")]
impl FileDialog {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self
    }

    #[must_use]
    pub(crate) fn set_title(self, _title: impl AsRef<str>) -> Self {
        self
    }

    #[must_use]
    pub(crate) fn set_directory(self, _directory: impl AsRef<Path>) -> Self {
        self
    }

    #[must_use]
    pub(crate) fn set_file_name(self, _file_name: impl AsRef<str>) -> Self {
        self
    }

    #[must_use]
    pub(crate) fn add_filter<S, I>(self, _name: impl AsRef<str>, _extensions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self
    }

    pub(crate) fn pick_folder(self) -> Option<PathBuf> {
        None
    }

    pub(crate) fn pick_file(self) -> Option<PathBuf> {
        None
    }

    pub(crate) fn pick_files(self) -> Option<Vec<PathBuf>> {
        None
    }

    pub(crate) fn save_file(self) -> Option<PathBuf> {
        None
    }
}
