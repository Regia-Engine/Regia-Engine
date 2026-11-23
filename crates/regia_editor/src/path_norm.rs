// SPDX-License-Identifier: MPL-2.0
//! Cross-platform path comparison for the editor (ADR-4824).
//!
//! Rust's [`Path::strip_prefix`] and `Path == Path` compare components exactly: never
//! case-folded, and `\` is not a separator on Linux/macOS. Windows file APIs case-fold, so a
//! level opened as `c:\dev\game\assets\maps\arena.level.ron` while the project root is
//! `C:/Dev/Game` names a file the filesystem opens happily, yet an exact prefix compare fails
//! and code degrades to "not under the project". These helpers normalize both sides before
//! comparing: ASCII case-folding where the host filesystem case-folds (Windows only — on
//! other filesystems `Rock.glb` and `rock.glb` may be two real files), and `/`/`\` separator
//! unification when one side is a stored asset string rather than a host path.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Whether path comparison case-folds on this host: Windows file APIs fold; Linux and
/// (possibly case-sensitive) macOS volumes do not.
pub(crate) const FOLD_CASE: bool = cfg!(windows);

fn component_eq(a: &OsStr, b: &OsStr, fold: bool) -> bool {
    if fold { a.eq_ignore_ascii_case(b) } else { a == b }
}

/// Strips `base` from `path` like [`Path::strip_prefix`], but comparing components with ASCII
/// case-folding when `fold`. `fold` is [`FOLD_CASE`] at call sites; it is a parameter so the
/// Windows comparison is exercised on every host in tests.
pub(crate) fn strip_prefix_fold(base: &Path, path: &Path, fold: bool) -> Option<PathBuf> {
    if let Ok(rel) = path.strip_prefix(base) {
        return Some(rel.to_path_buf());
    }
    if !fold {
        return None;
    }
    let mut rest = path.components();
    for expected in base.components() {
        let actual = rest.next()?;
        if !component_eq(actual.as_os_str(), expected.as_os_str(), true) {
            return None;
        }
    }
    Some(rest.as_path().to_path_buf())
}

/// [`strip_prefix_fold`] with the host's folding rule. When both paths exist they are
/// canonicalized first: a case-insensitive volume (the owner's macOS test host, most
/// external Windows volumes) resolves both sides to the on-disk spelling, which also covers
/// non-ASCII folds the ASCII compare misses; the component fold below stays the fallback
/// for paths that do not exist (and for exact-match volumes).
pub(crate) fn strip_prefix_normalized(base: &Path, path: &Path) -> Option<PathBuf> {
    if let (Ok(abs_base), Ok(abs_path)) = (base.canonicalize(), path.canonicalize())
        && let Ok(rel) = abs_path.strip_prefix(&abs_base)
    {
        return Some(rel.to_path_buf());
    }
    strip_prefix_fold(base, path, FOLD_CASE)
}

/// Whether `path` ends with exactly the components of `suffix` (a segment-boundary suffix
/// match, unlike `str::ends_with` on slash-joined strings).
pub(crate) fn ends_with_components(path: &Path, suffix: &Path, fold: bool) -> bool {
    let whole: Vec<_> = path.components().collect();
    let tail: Vec<_> = suffix.components().collect();
    whole.len() >= tail.len()
        && whole[whole.len() - tail.len()..]
            .iter()
            .zip(&tail)
            .all(|(a, b)| component_eq(a.as_os_str(), b.as_os_str(), fold))
}

/// Whether `a` and `b` denote the same existing file: canonicalized compare (resolves `.`,
/// `..`, symlinks and, on case-insensitive filesystems, the on-disk spelling), falling back
/// to a component compare for paths that do not exist.
pub(crate) fn same_file(a: &Path, b: &Path, fold: bool) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => {
            let ca: Vec<_> = a.components().collect();
            let cb: Vec<_> = b.components().collect();
            ca.len() == cb.len()
                && ca
                    .iter()
                    .zip(&cb)
                    .all(|(x, y)| component_eq(x.as_os_str(), y.as_os_str(), fold))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_prefix_fold_accepts_case_variant_components() {
        // fold=true is the Windows comparison; proven here on every host.
        let base = Path::new("C:/Dev/Game/assets");
        let level = Path::new("c:/dev/game/assets/maps/arena.level.ron");
        assert_eq!(
            strip_prefix_fold(base, level, true).as_deref(),
            Some(Path::new("maps/arena.level.ron")),
            "a case-variant root still strips (the level opens on Windows)"
        );
        assert_eq!(
            strip_prefix_fold(base, level, false),
            None,
            "the exact compare stays case-sensitive"
        );
        assert_eq!(
            strip_prefix_fold(base, Path::new("C:/Dev/Game/assets"), true).as_deref(),
            Some(Path::new("")),
            "an exact match needs no folding"
        );
        assert_eq!(
            strip_prefix_fold(base, Path::new("c:/dev/game/assets2/x"), true),
            None,
            "a shared prefix that is not a whole component does not strip"
        );
    }

    #[test]
    fn ends_with_components_needs_a_segment_boundary() {
        let shown = Path::new("/p/assets/maps/x.tilemap.ron");
        assert!(ends_with_components(
            shown,
            Path::new("maps/x.tilemap.ron"),
            false
        ));
        assert!(ends_with_components(
            shown,
            Path::new("assets/maps/x.tilemap.ron"),
            false
        ));
        assert!(!ends_with_components(
            shown,
            Path::new("oldmaps/x.tilemap.ron"),
            false
        ));
        assert!(!ends_with_components(
            shown,
            Path::new("x.tilemap.ron.bak"),
            false
        ));
        assert!(!ends_with_components(
            Path::new("/p/x.tilemap.ron"),
            Path::new("maps/x.tilemap.ron"),
            false
        ));
        assert!(
            ends_with_components(
                Path::new("/p/assets/MAPS/X.TILEMAP.RON"),
                Path::new("maps/x.tilemap.ron"),
                true
            ),
            "the Windows compare folds case"
        );
    }

    #[test]
    fn same_file_accepts_respelled_existing_paths() {
        let dir = regia_test_utils::unique_temp_dir("path_norm_same_file");
        std::fs::create_dir_all(dir.join("sub")).expect("dir");
        regia_test_utils::write_atomic(&dir.join("level.tilemap.ron"), b"x").expect("write");
        let a = dir.join("level.tilemap.ron");
        let b = dir.join("sub/../level.tilemap.ron");
        assert!(same_file(&a, &b, false), "a `sub/../` spelling is the same file");
        assert!(!same_file(&a, &dir.join("sub/nope.ron"), false));
    }

    /// Audit G editor4-platform L5 (sprint-fixrev-08-k): existing paths are canonicalized
    /// before the component compare, so a `sub/../` spelling strips on every host (on an
    /// exact-match volume like this one the plain fold would reject it), and a path under a
    /// case-variant of an existing directory strips too (on a case-insensitive volume the
    /// canonicalize resolves the spelling; the assertion only requires the exact volume not
    /// to mis-strip).
    #[test]
    fn strip_prefix_normalized_resolves_existing_paths_first() {
        let dir = regia_test_utils::unique_temp_dir("path_norm_strip_normalized");
        std::fs::create_dir_all(dir.join("assets/maps")).expect("dirs");
        let base = dir.join("assets");
        let dotted = dir.join("assets/sub/../maps");
        std::fs::create_dir_all(&dotted).expect("dotted dir");
        assert_eq!(
            strip_prefix_normalized(&base, &dotted).as_deref(),
            Some(std::path::Path::new("maps")),
            "a `sub/../` spelling of an existing directory strips via canonicalize"
        );
        assert!(
            strip_prefix_normalized(&base, &dir.join("assets2/x")).is_none(),
            "a sibling directory does not strip"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit G editor4-platform L6 (sprint-fixrev-08-k): the call sites fold exactly when
    /// the host filesystem folds; flipping `FOLD_CASE` or dropping the cfg must fail this
    /// on every host, not silently reopen the Windows findings on Linux CI.
    #[test]
    fn fold_case_matches_the_host() {
        assert_eq!(FOLD_CASE, cfg!(windows));
    }
}
