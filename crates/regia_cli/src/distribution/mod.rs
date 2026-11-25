// SPDX-License-Identifier: MPL-2.0
//! Engine distribution and upgrade for game developers (`docs/specs/94-distribution-upgrade.md`, ADR-1852).
//!
//! - [`EngineSource`]: where a project's engine crates come from (a path, a git tag, a registry).
//! - [`pin`]: reading and rewriting a project's engine dependencies and its `Regia.toml` line.
//! - [`release`]: the packaged engine release (`regia-engine.release.ron`) and its verification.
//!
//! The version policy itself is `regia_core::engine_version` (pure, every target).

use std::fmt;
use std::path::{Path, PathBuf};

use regia_core::engine_version::EngineVersion;

pub mod lock;
pub mod pin;
pub mod release;

pub use lock::{lock_drift, seed_lockfile};

pub use pin::{
    DepSite, DepSource, EnginePin, PinEdit, read_pin, read_requirement, rewrite_pin, stamp_engine_line,
};
pub use release::{
    CHECKSUMS_FILE, EngineRelease, PackagedRelease, RELEASE_FORMAT_VERSION, RELEASE_MANIFEST, ReleaseFile,
    ReleaseReport, checksum_files, checksums_text, package_release, read_release, verify_checksums,
    verify_release,
};

/// `release-chunks` format version (ADR-8404). Re-exported so `xtask` can hash the schema.
pub use regia_release_chunks::FORMAT_VERSION as RELEASE_CHUNKS_FORMAT_VERSION;
/// `release-signature` format version (the `.sig` sidecar, ADR-8846). Re-exported for `xtask`.
pub use regia_release_chunks::RELEASE_SIGNATURE_FORMAT;
/// Serde field list of `release-chunks` version 1. `cargo xtask check-formats` hashes this.
pub use regia_release_chunks::schema_fingerprint as release_chunks_schema_fingerprint;
/// Serde field list of `release-signature` version 1. `cargo xtask check-formats` hashes this.
pub use regia_release_chunks::signature_schema_fingerprint as release_signature_schema_fingerprint;

/// A distribution failure with its stable code (`book/agents/distribution-upgrade.md`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DistError {
    /// Lowercase ASCII, digits and `_`.
    pub code: &'static str,
    /// What went wrong and, where it helps, what to do.
    pub message: String,
}

impl DistError {
    /// An error with `code`.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// An IO failure on `path` (`io_error`).
    pub fn io(path: &Path, err: &std::io::Error) -> Self {
        Self::new("io_error", format!("{}: {err}", path.display()))
    }
}

impl fmt::Display for DistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for DistError {}

/// How bad a [`Problem`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// The command fails (non-zero exit).
    Error,
    /// Reported; the command still succeeds.
    Warning,
}

impl Severity {
    /// `"error"` or `"warning"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// One finding of a distribution command, as it appears in the JSON document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// Stable code.
    pub code: &'static str,
    /// Error or warning.
    pub severity: Severity,
    /// Human-readable detail.
    pub message: String,
}

impl Problem {
    /// An error finding.
    pub fn error(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            severity: Severity::Error,
            message: message.into(),
        }
    }

    /// A warning finding.
    pub fn warning(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            severity: Severity::Warning,
            message: message.into(),
        }
    }

    /// `{ "code", "severity", "message" }`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "code": self.code,
            "severity": self.severity.as_str(),
            "message": self.message,
        })
    }
}

impl From<DistError> for Problem {
    fn from(err: DistError) -> Self {
        Self::error(err.code, err.message)
    }
}

/// Where a project's engine crates come from (`--engine-source` / `--engine`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineSource {
    /// An engine checkout or a packaged release directory. Absolute paths are written as they are;
    /// a relative path is relative to the project root (a vendored copy) and is written relative to
    /// each manifest that names it.
    Path(PathBuf),
    /// A git repository at a tag.
    Git {
        /// Repository URL.
        url: String,
        /// Tag name (e.g. `v0.2.5`).
        tag: String,
    },
    /// A private Cargo registry at an exact version.
    Registry {
        /// Registry name (`[registries.<name>]` in the game developer's Cargo config).
        name: String,
        /// Engine version every engine crate is pinned to (`=VERSION`).
        version: EngineVersion,
    },
}

impl EngineSource {
    /// Parses `path:<dir>`, `git:<url>#<tag>` or `registry:<name>[@<version>]`. A registry without a
    /// version takes `engine`.
    ///
    /// # Errors
    ///
    /// `engine_source_invalid` for any other shape, an empty part, or a quote or newline (the value
    /// goes into a TOML string).
    pub fn parse(text: &str, engine: EngineVersion) -> Result<Self, DistError> {
        let invalid = |why: &str| {
            DistError::new(
                "engine_source_invalid",
                format!(
                    "engine source `{text}` {why}; expected `path:<dir>`, `git:<url>#<tag>` or \
                     `registry:<name>[@<version>]`"
                ),
            )
        };
        if text.contains(['"', '\n', '\r']) {
            return Err(invalid("contains a quote or a newline"));
        }
        let Some((kind, rest)) = text.split_once(':') else {
            return Err(invalid("has no kind"));
        };
        match kind {
            "path" if !rest.is_empty() => Ok(Self::Path(PathBuf::from(rest))),
            "git" => match rest.rsplit_once('#') {
                Some((url, tag)) if !url.is_empty() && !tag.is_empty() => Ok(Self::Git {
                    url: url.to_string(),
                    tag: tag.to_string(),
                }),
                _ => Err(invalid("needs `#<tag>`")),
            },
            "registry" => {
                let (name, version) = match rest.split_once('@') {
                    Some((name, version)) => (
                        name,
                        EngineVersion::parse(version).map_err(|err| invalid(&err.to_string()))?,
                    ),
                    None => (rest, engine),
                };
                let valid_name = !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
                if valid_name {
                    Ok(Self::Registry {
                        name: name.to_string(),
                        version,
                    })
                } else {
                    Err(invalid("has an empty or invalid registry name"))
                }
            }
            _ => Err(invalid("has an unknown kind or an empty value")),
        }
    }

    /// The engine directory on this machine for a path source (a relative path is resolved
    /// against `project`); `None` for git and registry sources.
    pub fn local_dir(&self, project: &Path) -> Option<PathBuf> {
        match self {
            Self::Path(dir) if dir.is_absolute() || is_absolute_text(&slash_path(dir)) => Some(dir.clone()),
            Self::Path(dir) => Some(project.join(dir)),
            Self::Git { .. } | Self::Registry { .. } => None,
        }
    }

    /// `"path"`, `"git"` or `"registry"`.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Path(_) => "path",
            Self::Git { .. } => "git",
            Self::Registry { .. } => "registry",
        }
    }
}

impl fmt::Display for EngineSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(path) => write!(f, "path:{}", slash_path(path)),
            Self::Git { url, tag } => write!(f, "git:{url}#{tag}"),
            Self::Registry { name, version } => write!(f, "registry:{name}@{version}"),
        }
    }
}

/// `path` with `/` separators (Cargo accepts them on every host), and without Windows' verbatim
/// `\\?\` prefix on a drive path: Cargo refuses `path = "//?/C:/…"` with "invalid path url"
/// (ADR-10616).
pub fn slash_path(path: &Path) -> String {
    regia_core::project_path::strip_verbatim(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// True for a `/`-rooted path or a Windows drive path (`C:/…`, `C:\…`) on any host.
pub fn is_absolute_text(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with('/')
        || path.starts_with('\\')
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

/// The version of an engine directory: its release manifest's version, else its workspace
/// `Cargo.toml [workspace.package] version`. `None` when neither is readable.
pub fn engine_dir_version(dir: &Path) -> Option<EngineVersion> {
    if let Ok(release) = read_release(dir) {
        return EngineVersion::parse(&release.version).ok();
    }
    let text = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
    let table: toml::Table = toml::from_str(&text).ok()?;
    let version = table.get("workspace")?.get("package")?.get("version")?.as_str()?;
    EngineVersion::parse(version).ok()
}

/// True for a directory that holds an engine (a packaged release or a checkout): project walks
/// (pins, format migrators) never descend into one.
pub fn is_engine_tree(dir: &Path) -> bool {
    dir.join(RELEASE_MANIFEST).is_file() || dir.join("crates/regia/Cargo.toml").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    const V: EngineVersion = EngineVersion::new(0, 2, 5);

    #[test]
    fn sources_parse_and_display_round_trip() {
        for text in [
            "path:/opt/regia-engine-0.2.5",
            "path:vendor/regia-engine-0.2.5",
            "git:https://example.com/regia.git#v0.2.5",
            "git:ssh://git@example.com:22/regia.git#v0.2.5",
            "registry:studio@0.2.5",
        ] {
            let source = EngineSource::parse(text, V).expect(text);
            assert_eq!(source.to_string(), text);
        }
        assert_eq!(
            EngineSource::parse("registry:studio", V).expect("registry"),
            EngineSource::Registry {
                name: "studio".into(),
                version: V
            }
        );
    }

    #[test]
    fn bad_sources_are_rejected_with_the_code() {
        for text in [
            "",
            "/opt/regia",
            "path:",
            "git:https://x",
            "git:#v1",
            "git:https://x#",
            "registry:",
            "registry:bad name",
            "registry:s@0.2",
            "ftp:x",
            "path:/a\"b",
        ] {
            let err = EngineSource::parse(text, V).expect_err(text);
            assert_eq!(err.code, "engine_source_invalid", "{text}");
        }
    }

    #[test]
    fn absolute_paths_are_recognized_on_any_host() {
        assert!(is_absolute_text("/opt/x"));
        assert!(is_absolute_text("C:/work/regia"));
        assert!(is_absolute_text("d:\\work"));
        assert!(!is_absolute_text("vendor/regia"));
        assert!(!is_absolute_text("../regia"));
    }
}
