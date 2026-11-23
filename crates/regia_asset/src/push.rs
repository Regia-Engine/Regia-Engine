// SPDX-License-Identifier: MPL-2.0
//! Assets pushed into a running game (`assets.push`, P-301, ADR-7836, `book/agents/asset-push.md`).
//!
//! The editor sends a changed asset's bytes to a game it launched. This module is the game side's
//! file half: it writes the bytes where the game reads them, and only there.
//!
//! - A **cooked** asset (`<id>.fckd`) goes into the server's cooked cache under its id's file name,
//!   so no caller-chosen path is ever joined. The id is then served from the cache even when a
//!   mounted pak holds it ([`PushedIds`]): a packaged game reads its paks first, and a push must win
//!   over them until the game restarts.
//! - A **source** file the runtime reads directly (a UI document, for example) goes under the
//!   project's `assets/` directory, at a relative path [`confine_source_path`] accepted.
//!
//! Every write is atomic (a temporary file in the same directory, then a rename) and skipped when
//! the file already holds the same bytes. The size cap is [`PUSH_MAX_BYTES`].

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::id::AssetId;

/// The largest push accepted, in decoded bytes (16 MiB). Its base64 text fits in one agent
/// message (32 MiB).
pub const PUSH_MAX_BYTES: usize = 16 << 20;

/// The directory under the project root that source pushes are confined to.
pub const SOURCE_PUSH_ROOT: &str = "assets";

/// Why a push was refused or failed.
#[derive(Debug)]
pub enum PushError {
    /// More than [`PUSH_MAX_BYTES`] bytes.
    TooLarge(usize),
    /// A source path that is empty, absolute, has `..`, `.` or a drive prefix, or does not lie
    /// under `assets/`.
    BadPath(String),
    /// The write failed.
    Io(std::io::Error),
}

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(len) => write!(f, "{len} bytes is over the push limit of {PUSH_MAX_BYTES}"),
            Self::BadPath(why) => f.write_str(why),
            Self::Io(e) => write!(f, "write failed: {e}"),
        }
    }
}

impl std::error::Error for PushError {}

/// What a push wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushOutcome {
    /// The file written (or already holding the bytes).
    pub path: PathBuf,
    /// `false` when the file already held exactly these bytes (nothing was written).
    pub changed: bool,
    /// The byte count.
    pub bytes: usize,
}

/// Ids pushed into this game: they are read from the cooked cache before any mounted pak. Shared
/// by an [`crate::AssetServer`] and every [`crate::PayloadReader`] it made.
#[derive(Debug, Default)]
pub struct PushedIds(RwLock<HashSet<AssetId>>);

impl PushedIds {
    /// A shared, empty set.
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::default()
    }

    /// Whether `id` was pushed.
    #[must_use]
    pub fn contains(&self, id: AssetId) -> bool {
        self.0.read().is_ok_and(|set| set.contains(&id))
    }

    /// Records `id` as pushed.
    pub fn insert(&self, id: AssetId) {
        match self.0.write() {
            Ok(mut set) => {
                set.insert(id);
            }
            Err(poisoned) => {
                poisoned.into_inner().insert(id);
            }
        }
    }

    /// How many ids were pushed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.read().map_or(0, |set| set.len())
    }

    /// Whether nothing was pushed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `rel` (a `/`- or `\`-separated path relative to the project) as a path under
/// `<root>/assets/`, or why it is refused: empty, absolute, a drive or UNC prefix, a `.` or `..`
/// component, or a first component other than `assets`. `assets/` itself is refused (it is a
/// directory). Nothing touches the file system.
///
/// # Errors
///
/// [`PushError::BadPath`] with the reason.
pub fn confine_source_path(root: &Path, rel: &str) -> Result<PathBuf, PushError> {
    let bad = |why: &str| Err(PushError::BadPath(format!("source path {rel:?}: {why}")));
    if rel.is_empty() {
        return bad("empty");
    }
    if rel.contains('\0') {
        return bad("has a NUL byte");
    }
    let normalized = rel.replace('\\', "/");
    if normalized.starts_with('/') || normalized.contains(':') {
        return bad("must be relative to the project (no leading `/`, no drive or scheme)");
    }
    let mut parts = normalized.split('/').filter(|part| !part.is_empty());
    if parts.next() != Some(SOURCE_PUSH_ROOT) {
        return bad("must lie under assets/");
    }
    let mut out = root.join(SOURCE_PUSH_ROOT);
    let mut depth = 0usize;
    for part in parts {
        // Re-check each part with the platform's own parser: only plain names pass.
        let mut components = Path::new(part).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(name)), None) if part != "." && part != ".." => out.push(name),
            _ => return bad("`.` and `..` components are not allowed"),
        }
        depth += 1;
    }
    if depth == 0 {
        return bad("names the assets/ directory, not a file");
    }
    Ok(out)
}

/// Writes `bytes` to `path` atomically (a sibling temporary file, then a rename), creating the
/// parent directory. Skips the write when the file already holds exactly `bytes`.
///
/// # Errors
///
/// [`PushError::TooLarge`] over the cap; [`PushError::Io`] when the write or the rename failed.
pub fn write_pushed(path: &Path, bytes: &[u8]) -> Result<PushOutcome, PushError> {
    if bytes.len() > PUSH_MAX_BYTES {
        return Err(PushError::TooLarge(bytes.len()));
    }
    let outcome = |changed| PushOutcome {
        path: path.to_path_buf(),
        changed,
        bytes: bytes.len(),
    };
    if std::fs::metadata(path).is_ok_and(|m| m.len() == bytes.len() as u64)
        && std::fs::read(path).is_ok_and(|old| old == bytes)
    {
        return Ok(outcome(false));
    }
    let parent = path
        .parent()
        .ok_or_else(|| PushError::BadPath(format!("{} has no parent directory", path.display())))?;
    std::fs::create_dir_all(parent).map_err(PushError::Io)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!(".{name}.push-{}.tmp", std::process::id()));
    let written = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(PushError::Io(e));
    }
    Ok(outcome(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_paths_are_confined_to_assets() {
        let root = Path::new("/game");
        let ok = confine_source_path(root, "assets/ui/hud.ui.ron").expect("plain path");
        assert_eq!(ok, root.join("assets").join("ui").join("hud.ui.ron"));
        let back = confine_source_path(root, "assets\\ui\\menu.ui.ron").expect("backslashes");
        assert_eq!(back, root.join("assets").join("ui").join("menu.ui.ron"));
        for bad in [
            "",
            "/etc/passwd",
            "assets/../Regia.toml",
            "assets/ui/../../x",
            "assets/./x",
            "ui/hud.ui.ron",
            "assets",
            "assets/",
            "C:/Windows/x",
            "assets/c:x",
            "\\\\server\\share\\x",
            "assets/a\0b",
        ] {
            assert!(
                matches!(confine_source_path(root, bad), Err(PushError::BadPath(_))),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn writes_are_atomic_skip_identical_bytes_and_respect_the_cap() {
        let tmp = regia_test_utils::unique_temp_dir("asset-push-write");
        let path = tmp.join("cache").join("x.fckd");
        let first = write_pushed(&path, b"one").expect("first write");
        assert!(first.changed);
        assert_eq!(std::fs::read(&path).expect("read"), b"one");
        let same = write_pushed(&path, b"one").expect("same bytes");
        assert!(!same.changed, "identical bytes are not rewritten");
        let second = write_pushed(&path, b"two!").expect("new bytes");
        assert!(second.changed);
        assert_eq!(std::fs::read(&path).expect("read"), b"two!");
        let leftovers: Vec<_> = std::fs::read_dir(tmp.join("cache"))
            .expect("dir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temporary file is left behind");
        let big = vec![0u8; PUSH_MAX_BYTES + 1];
        assert!(matches!(write_pushed(&path, &big), Err(PushError::TooLarge(_))));
        assert_eq!(
            std::fs::read(&path).expect("read"),
            b"two!",
            "a refused push writes nothing"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn pushed_ids_are_shared() {
        let ids = PushedIds::shared();
        let other = Arc::clone(&ids);
        let id = AssetId::new_random();
        assert!(ids.is_empty());
        other.insert(id);
        assert!(ids.contains(id));
        assert_eq!(ids.len(), 1);
    }
}
