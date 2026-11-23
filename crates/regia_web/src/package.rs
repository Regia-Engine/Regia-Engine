// SPDX-License-Identifier: MPL-2.0
//! A fetched package: the byte-level check, with `regia_viewer_pack`'s refusals turned into the player's
//! codes. The browser fetches the files; this needs no browser.

use std::collections::BTreeMap;

use regia_viewer_pack::{MANIFEST_FILE, Manifest, PackError, Package};

use crate::{WebError, urls};

/// The name the manifest has in a package folder.
pub const MANIFEST: &str = MANIFEST_FILE;

/// The player's code for a refused package.
pub fn web_error(e: PackError) -> WebError {
    let code = match e.code {
        "pack_hash_mismatch" | "pack_missing_file" => "package_hash_mismatch",
        "pack_version" => "package_version_unsupported",
        _ => "package_invalid",
    };
    WebError::new(code, e.to_string())
}

/// Reads a manifest and lists what must be fetched: `(file name, its URL)`, in manifest order. Nothing is
/// fetched from a manifest that fails its checks, and its names cannot leave the folder.
pub fn files_to_fetch(folder: &str, manifest: &[u8]) -> Result<Vec<(String, String)>, WebError> {
    let parsed = Manifest::parse(manifest).map_err(web_error)?;
    parsed
        .files
        .iter()
        .map(|f| Ok((f.path.clone(), urls::join(folder, &f.path)?)))
        .collect()
}

/// Checks every file's size and hash against the manifest, then decodes.
pub fn verify(manifest: &[u8], files: &BTreeMap<String, Vec<u8>>) -> Result<Package, WebError> {
    Package::from_files(manifest, |name| files.get(name).map(Vec::as_slice)).map_err(web_error)
}
