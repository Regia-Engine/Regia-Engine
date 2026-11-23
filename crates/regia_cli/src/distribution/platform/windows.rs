// SPDX-License-Identifier: MPL-2.0
//! Windows release-source link classification (docs/04 § 8).

/// Whether a source entry is a symlink or another reparse point, including a junction.
pub(super) fn is_link(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_type().is_symlink() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}
