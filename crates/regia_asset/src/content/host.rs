// SPDX-License-Identifier: MPL-2.0
//! `content-host.ron` (format `content-host` v1): where a packaged game fetches `external` chunks
//! (`docs/specs/40-content-chunks-dlc-asset-packs.md` § 2.6, ADR-1876). The cook writes it from
//! `Regia.toml` `[content] base_url` next to the content manifest, because a package does not ship
//! `Regia.toml`. `REGIA_CONTENT_BASE_URL` overrides it for one run.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::ContentError;
use super::manifest::write_atomic;

/// Version of `content-host.ron` this build reads and writes.
pub const CONTENT_HOST_FORMAT_VERSION: u32 = 1;
/// File name of the host record, next to the content manifest.
pub const CONTENT_HOST_FILE: &str = "content-host.ron";
/// Environment variable that overrides the base URL for one run.
pub const CONTENT_BASE_URL_ENV: &str = "REGIA_CONTENT_BASE_URL";

/// The serde field-list fingerprint of `content-host.ron` (hashed by `cargo xtask check-formats`).
#[must_use]
pub fn content_host_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<ContentHost>()])
}

/// The contents of `content-host.ron`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentHost {
    /// Format version.
    pub format_version: u32,
    /// `http://` or `https://` URL; chunks live at `<base_url>/<build_id>/<file>`.
    pub base_url: String,
}

impl ContentHost {
    /// A record for `base_url`.
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            format_version: CONTENT_HOST_FORMAT_VERSION,
            base_url: base_url.into(),
        }
    }

    /// Reads the record in `dir`; `Ok(None)` when there is none.
    ///
    /// # Errors
    /// The file does not read or parse, or it is newer than this build.
    pub fn read_from_dir(dir: &Path) -> Result<Option<Self>, ContentError> {
        let text = match std::fs::read_to_string(dir.join(CONTENT_HOST_FILE)) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(ContentError::Io(e.to_string())),
        };
        Self::from_ron_str(&text).map(Some)
    }

    /// Parses the text of a `content-host.ron` (what [`ContentHost::read_from_dir`] reads; the fuzz
    /// registry feeds it directly).
    ///
    /// # Errors
    /// The text does not parse, or it is newer than this build.
    pub fn from_ron_str(text: &str) -> Result<Self, ContentError> {
        let host: Self =
            ron::from_str(text).map_err(|e| ContentError::Format(format!("{CONTENT_HOST_FILE}: {e}")))?;
        if host.format_version > CONTENT_HOST_FORMAT_VERSION {
            return Err(ContentError::ManifestVersion {
                found: host.format_version,
                supported: CONTENT_HOST_FORMAT_VERSION,
            });
        }
        Ok(host)
    }

    /// The record as RON text (what [`ContentHost::write_to_dir`] writes).
    ///
    /// # Errors
    /// RON cannot write it.
    pub fn to_ron_string(&self) -> Result<String, ContentError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| ContentError::Format(e.to_string()))
    }

    /// Writes the record into `dir`, atomically.
    ///
    /// # Errors
    /// The file does not write.
    pub fn write_to_dir(&self, dir: &Path) -> Result<(), ContentError> {
        let text = self.to_ron_string()?;
        write_atomic(&dir.join(CONTENT_HOST_FILE), text.as_bytes())
    }
}

/// The base URL of this run: [`CONTENT_BASE_URL_ENV`], else `content-host.ron` in `content_dir`.
#[must_use]
pub fn resolve_base_url(content_dir: &Path) -> Option<String> {
    pick_base_url(regia_core::brand::env_var(CONTENT_BASE_URL_ENV).ok(), content_dir)
}

/// `from_env` when set and not empty, else `content-host.ron` in `content_dir` (a record that does
/// not read is logged and ignored).
#[must_use]
pub fn pick_base_url(from_env: Option<String>, content_dir: &Path) -> Option<String> {
    from_env
        .filter(|url| !url.is_empty())
        .or_else(|| match ContentHost::read_from_dir(content_dir) {
            Ok(host) => host.map(|h| h.base_url),
            Err(e) => {
                tracing::warn!(target: "regia_asset", "{CONTENT_HOST_FILE} not used: {e}");
                None
            }
        })
}
