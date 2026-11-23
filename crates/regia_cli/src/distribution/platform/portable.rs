// SPDX-License-Identifier: MPL-2.0
//! Portable release-source link classification.

/// Whether a source entry is a symbolic link.
pub(super) fn is_link(meta: &std::fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}
