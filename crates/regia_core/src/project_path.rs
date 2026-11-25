// SPDX-License-Identifier: MPL-2.0
//! Confining caller-supplied relative paths to a project root (ADR-1816).
//!
//! Agent requests, CLI arguments and content files name project files by relative path. Joining
//! such a string onto a root is not enough: `root.join("/etc/x")` (or `C:\x`, `\\server\share`)
//! replaces the root, `a/../../x` climbs out, and a symlink inside the project can lead anywhere.
//! Every untrusted relative path goes through [`resolve_read`] or [`resolve_write`] instead.
//!
//! The lexical rules ([`check_relative`]) are the same on every platform, so a request that a Linux
//! host refuses is refused on Windows and Android too:
//! - not empty, no NUL byte, no `:` (drive letters, NTFS alternate streams, URL schemes);
//! - no leading `/` or `\` (absolute, root-relative, UNC `\\server\share`, `\\?\` verbatim);
//! - no `..` segment (both `/` and `\` separate segments), nor one of only dots and spaces (`.. `,
//!   `...`), which no Windows file can be named;
//! - no segment ending in a dot or a space: Windows opens `a.txt.` and `.regia./x` as `a.txt` and
//!   `.regia/x`, so the name a check sees is not the file the open gets (ADR-3206);
//! - no Windows device name segment (`CON`, `NUL`, `COM1`, `LPT1`, `COM¹`, …, with any extension).
//!
//! Then the filesystem check: the root is canonicalised, and the deepest existing ancestor of the
//! joined path (the file itself when it exists) is canonicalised with symlinks resolved; it must lie
//! under the canonical root. Neither function creates anything.
//!
//! The filesystem check holds when it runs. A process that can write inside the project and swaps a
//! checked folder for a symlink between the check and the caller's open can still redirect that
//! open; returning the canonical path would not close that window (its folders can be swapped as
//! well). The threat these functions answer is a hostile *path string* from a request or a content
//! file; a hostile concurrent writer inside the project needs handle-relative opens.
//!
//! The table `crates/regia_core/tests/windows_path_rules.rs` runs these rules as pure logic on every
//! host, because a level written on Linux gets opened on Windows.

use std::path::{Path, PathBuf};

/// Why a relative path was refused. [`ProjectPathError::code`] is a stable lowercase code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProjectPathError {
    /// The path is empty (or only `.` and separators).
    #[error("the path is empty")]
    Empty,
    /// The path holds a NUL byte.
    #[error("the path holds a NUL byte")]
    Nul,
    /// The path is absolute, root-relative or UNC (starts with `/` or `\`).
    #[error("the path must be relative to the project, not absolute")]
    Absolute,
    /// The path holds `:` (a drive letter such as `C:`, an alternate data stream, or a scheme).
    #[error("the path must not hold a drive letter or `:`")]
    Prefix,
    /// The path holds a `..` segment.
    #[error("the path must not hold `..`")]
    ParentDir,
    /// A segment is a Windows device name (`CON`, `NUL`, `COM1`, …).
    #[error("the path names a reserved device (`{0}`)")]
    Device(String),
    /// A segment ends in a dot or a space. Windows drops those from the last segment, and one dot
    /// from any segment, when it opens the file.
    #[error("a path segment must not end in a dot or a space")]
    TrailingDotOrSpace,
    /// The path is a network, verbatim or device form (`\\\\server\\share`, `\\\\?\\`, `\\\\.\\`) outside
    /// every allowed root. It is refused before the filesystem is touched.
    #[error("the path must not be a network or device path outside the allowed roots")]
    Remote,
    /// The path resolves (through a symlink) outside the project root.
    #[error("the path leads outside the project")]
    Escapes,
    /// The project root or the path could not be resolved.
    #[error("cannot resolve the path: {0}")]
    Io(String),
}

impl ProjectPathError {
    /// A stable lowercase protocol code for the refusal (`path_absolute`, `path_escapes`, …).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Empty => "path_empty",
            Self::Nul => "path_nul",
            Self::Absolute => "path_absolute",
            Self::Prefix => "path_prefix",
            Self::ParentDir => "path_parent",
            Self::Device(_) => "path_device",
            Self::TrailingDotOrSpace => "path_trailing",
            Self::Remote => "path_remote",
            Self::Escapes => "path_escapes",
            Self::Io(_) => "path_io",
        }
    }
}

/// Windows device names, refused as a segment stem on every platform.
const DEVICES: [&str; 6] = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];

fn is_device(segment: &str) -> bool {
    // The extension and trailing dots and spaces do not count: Windows' naming documentation reserves
    // `nul.txt` along with `NUL`. Newer builds reserve less, and the file still has to open on older ones.
    let stem = segment
        .split('.')
        .next()
        .unwrap_or(segment)
        .trim_end_matches([' ', '.']);
    if DEVICES.iter().any(|d| stem.eq_ignore_ascii_case(d)) {
        return true;
    }
    // COM0..9 and LPT0..9, and the superscript forms Windows reserves as well (`COM¹`, `LPT³`).
    let Some(head) = stem.get(..3) else {
        return false;
    };
    if !head.eq_ignore_ascii_case("COM") && !head.eq_ignore_ascii_case("LPT") {
        return false;
    }
    let mut tail = stem[3..].chars();
    matches!(
        (tail.next(), tail.next()),
        (Some('0'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}'), None)
    )
}

/// Whether `path` starts with two separators in any mix: a Windows UNC (`\\\\server\\share`),
/// verbatim (`\\\\?\\`) or device (`\\\\.\\`) form. Merely stating `\\\\server\\share\\x` makes Windows
/// connect to `server` and offer the user's credentials, so nothing may probe such a path before
/// it is known to lie under an allowed root. Pure string logic, so every host runs the same rule.
#[must_use]
pub fn is_remote_form(path: &str) -> bool {
    let mut bytes = path.bytes();
    matches!(
        (bytes.next(), bytes.next()),
        (Some(b'/' | b'\\'), Some(b'/' | b'\\'))
    )
}

/// Checks `rel` lexically (no filesystem access) and returns it as a relative path built from its
/// `/`- or `\`-separated segments (`.` and empty segments dropped).
///
/// # Errors
/// The first rule `rel` breaks (see the module docs).
pub fn check_relative(rel: &str) -> Result<PathBuf, ProjectPathError> {
    if rel.contains('\0') {
        return Err(ProjectPathError::Nul);
    }
    if rel.starts_with(['/', '\\']) {
        return Err(ProjectPathError::Absolute);
    }
    if rel.contains(':') {
        return Err(ProjectPathError::Prefix);
    }
    let mut out = PathBuf::new();
    for seg in rel.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            // Only dots and spaces: `.. `, `...` and `. .` cannot be a file's name on Windows.
            s if s.trim_end_matches([' ', '.']).is_empty() => return Err(ProjectPathError::ParentDir),
            s if is_device(s) => return Err(ProjectPathError::Device(s.to_string())),
            s if s.ends_with(['.', ' ']) => return Err(ProjectPathError::TrailingDotOrSpace),
            s => out.push(s),
        }
    }
    if out.as_os_str().is_empty() {
        return Err(ProjectPathError::Empty);
    }
    Ok(out)
}

/// Reads `path`, which must be a regular file of at most `max` bytes. A link, a FIFO or a device in
/// its place is an error, and the read cannot block on one or run past `max`: this is for a file
/// whose name a project or a mirror controls (a token, a manifest). On Linux and Android the open
/// itself refuses a link and does not wait for a FIFO's writer, so a swap after the check gains
/// nothing; elsewhere the check before the open is the guard.
///
/// # Errors
/// [`std::io::ErrorKind::InvalidData`] for the wrong kind of file or a file over `max`, or the IO
/// error of the open or the read.
pub fn read_regular_capped(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let refuse = |why: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, why.to_string());
    if !path.symlink_metadata()?.file_type().is_file() {
        return Err(refuse("not a regular file"));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(refuse("not a regular file"));
    }
    if meta.len() > max {
        return Err(refuse("the file is larger than its limit"));
    }
    let mut bytes = Vec::new();
    file.take(max.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(refuse("the file is larger than its limit"));
    }
    Ok(bytes)
}

fn canonical_root(root: &Path) -> Result<PathBuf, ProjectPathError> {
    root.canonicalize()
        .map_err(|e| ProjectPathError::Io(format!("project root: {e}")))
}

/// Resolves `rel` for reading: [`check_relative`], then the joined path (symlinks resolved) must lie
/// under `root`. Returns the joined path (`root` as given plus the clean relative path).
///
/// # Errors
/// A lexical refusal, [`ProjectPathError::Escapes`], or [`ProjectPathError::Io`] when the root or
/// the file cannot be resolved (a missing file is `Io`).
pub fn resolve_read(root: &Path, rel: &str) -> Result<PathBuf, ProjectPathError> {
    // Join the checked components, not the caller's spelling: on Unix `\` is not a separator.
    let clean = check_relative(rel)?;
    let croot = canonical_root(root)?;
    let joined = root.join(clean);
    let resolved = joined
        .canonicalize()
        .map_err(|e| ProjectPathError::Io(e.to_string()))?;
    if !resolved.starts_with(&croot) {
        return Err(ProjectPathError::Escapes);
    }
    Ok(joined)
}

/// Resolves `rel` for writing: [`check_relative`], then the deepest existing ancestor of the joined
/// path (the file itself when it exists), symlinks resolved, must lie under `root`. Directories the
/// caller then creates below that ancestor stay inside the root. Creates nothing.
///
/// # Errors
/// A lexical refusal, [`ProjectPathError::Escapes`], or [`ProjectPathError::Io`] when the root
/// cannot be resolved.
pub fn resolve_write(root: &Path, rel: &str) -> Result<PathBuf, ProjectPathError> {
    let joined = root.join(check_relative(rel)?);
    check_under_root(root, &joined)?;
    Ok(joined)
}

/// Checks a path the caller already built under `root` (for example a sibling derived from a
/// resolved path): its deepest existing ancestor (the path itself when it exists), symlinks
/// resolved, must lie under the canonical `root`. A dangling symlink is refused.
///
/// # Errors
/// [`ProjectPathError::Escapes`], or [`ProjectPathError::Io`] when the root cannot be resolved.
pub fn check_under_root(root: &Path, path: &Path) -> Result<(), ProjectPathError> {
    let croot = canonical_root(root)?;
    // `symlink_metadata` sees a dangling symlink as existing; `canonicalize` then fails on it,
    // which refuses a link whose target cannot be checked.
    let mut probe = path;
    while probe.symlink_metadata().is_err() {
        match probe.parent() {
            Some(parent) => probe = parent,
            None => break,
        }
    }
    let resolved = probe.canonicalize().map_err(|_| ProjectPathError::Escapes)?;
    if resolved.starts_with(&croot) {
        Ok(())
    } else {
        Err(ProjectPathError::Escapes)
    }
}

/// The roots a path may lie under, canonicalised. The first root is the project and is taken as
/// given; a later one may come from a project's configuration, so one that names a whole filesystem
/// (`/`, `C:\\`) or a network or device form ([`is_remote_form`]) counts for nothing: the first would
/// allow every path, and resolving the second connects to the host it names.
fn canonical_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .iter()
        .enumerate()
        .filter(|(i, root)| *i == 0 || !is_remote_form(&root.to_string_lossy()))
        .filter_map(|(i, root)| {
            let canonical = root.canonicalize().ok()?;
            (i == 0 || canonical.parent().is_some()).then_some(canonical)
        })
        .collect()
}

/// Resolves a caller path against allowed roots (the agent's output-root rule): a relative path
/// passes [`check_relative`] and joins `roots[0]`; an absolute path is taken as given but may not
/// hold `..` or NUL, what follows its root must pass [`check_relative`] (the root's own spelling is
/// not held to the rules), and a network or device form ([`is_remote_form`]) must lie lexically under
/// a root before the filesystem is touched. Either way the deepest existing ancestor (the file
/// itself when it exists), symlinks resolved, must lie under one of the canonicalised `roots`.
/// Windows drive-relative (`C:x`) and root-relative (`\x`) paths are not absolute and fail
/// [`check_relative`]. Creates nothing.
///
/// # Errors
/// A lexical refusal, [`ProjectPathError::Remote`] or [`ProjectPathError::Escapes`] (outside every
/// root), or [`ProjectPathError::Io`] when no root can be resolved.
pub fn resolve_in_roots(roots: &[PathBuf], path: &str) -> Result<PathBuf, ProjectPathError> {
    use std::path::Component;
    let first = roots
        .first()
        .ok_or_else(|| ProjectPathError::Io("no allowed root".to_string()))?;
    let as_path = Path::new(path);
    let canonical = canonical_roots(roots);
    if canonical.is_empty() {
        return Err(ProjectPathError::Io("no allowed root exists".to_string()));
    }
    let joined = if as_path.is_absolute() {
        if path.contains('\0') {
            return Err(ProjectPathError::Nul);
        }
        if as_path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(ProjectPathError::ParentDir);
        }
        // The nearest root it lies under by spelling; what follows is held to the project rules.
        let tail = roots
            .iter()
            .enumerate()
            .filter(|(i, root)| *i == 0 || !is_remote_form(&root.to_string_lossy()))
            .map(|(_, root)| root)
            .chain(&canonical)
            .filter(|root| root.parent().is_some())
            .filter_map(|root| as_path.strip_prefix(root).ok())
            .min_by_key(|tail| tail.as_os_str().len());
        match tail {
            Some(tail) if tail.as_os_str().is_empty() => {}
            Some(tail) => drop(check_relative(&tail.to_string_lossy())?),
            None if is_remote_form(path) => return Err(ProjectPathError::Remote),
            None => {}
        }
        as_path.to_path_buf()
    } else {
        first.join(check_relative(path)?)
    };
    let mut probe = joined.as_path();
    while probe.symlink_metadata().is_err() {
        match probe.parent() {
            Some(parent) => probe = parent,
            None => break,
        }
    }
    let resolved = probe.canonicalize().map_err(|_| ProjectPathError::Escapes)?;
    if !canonical.iter().any(|r| resolved.starts_with(r)) {
        return Err(ProjectPathError::Escapes);
    }
    Ok(joined)
}

/// `path` without Windows' verbatim prefix on a drive path: `\\?\C:\x` becomes `C:\x`; every other
/// path (a verbatim UNC `\\?\UNC\…` path, any Unix path, a path that is not UTF-8) is returned
/// unchanged. `canonicalize` returns the verbatim form on Windows, and Cargo (`path = "//?/C:/…"`:
/// "invalid path url"), shortcuts and other programs refuse it, so a canonical path that is written
/// into a file or shown to the user goes through this first (ADR-10616). The rule is plain text, so it
/// holds the same on every host.
#[must_use]
pub fn strip_verbatim(path: &Path) -> PathBuf {
    match path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

/// [`std::fs::canonicalize`] through [`strip_verbatim`]: the absolute path with symlinks resolved,
/// spelled `C:\…` rather than `\\?\C:\…` on Windows (ADR-10616). For paths that are written into a
/// file or shown to the user; compare canonical paths with canonical paths.
///
/// # Errors
///
/// As [`std::fs::canonicalize`]: the path does not exist or cannot be resolved.
pub fn canonicalize_plain(path: &Path) -> std::io::Result<PathBuf> {
    path.canonicalize().map(|path| strip_verbatim(&path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_root(tag: &str) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-project-path-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp root");
        dir
    }

    /// ADR-10616: a verbatim drive path loses its prefix (the `regia new` Cargo.toml bug), and
    /// nothing else changes.
    #[test]
    fn verbatim_drive_paths_lose_their_prefix() {
        assert_eq!(strip_verbatim(Path::new(r"\\?\C:\x\y")), PathBuf::from(r"C:\x\y"));
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\C:\Users\me\AppData\Local\Regia\e")),
            PathBuf::from(r"C:\Users\me\AppData\Local\Regia\e")
        );
        for unchanged in [
            r"\\?\UNC\server\share",
            r"C:\x",
            "/home/me/e",
            r"\\?\",
            r"\\?\C",
            "rel/x",
        ] {
            assert_eq!(strip_verbatim(Path::new(unchanged)), PathBuf::from(unchanged));
        }
        let dir = temp_root("plain");
        let plain = canonicalize_plain(&dir).expect("canonical");
        assert!(plain.is_absolute());
        assert!(
            !plain.to_string_lossy().starts_with(r"\\?\"),
            "{}",
            plain.display()
        );
        assert_eq!(
            plain.canonicalize().expect("canonical"),
            dir.canonicalize().expect("canonical")
        );
        assert!(canonicalize_plain(&dir.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn resolved_path_uses_the_checked_components() {
        let root = temp_root("backslash");
        std::fs::create_dir_all(root.join("meshes")).expect("dir");
        std::fs::write(root.join("meshes").join("hero.gltf"), b"x").expect("file");
        assert_eq!(
            resolve_read(&root, "meshes\\hero.gltf").expect("resolves"),
            root.join("meshes").join("hero.gltf")
        );
        assert_eq!(
            resolve_write(&root, "meshes\\new.gltf").expect("resolves"),
            root.join("meshes").join("new.gltf")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn lexical_rules_refuse_every_escape_form() {
        let cases = [
            ("", ProjectPathError::Empty),
            ("./", ProjectPathError::Empty),
            ("a\0b", ProjectPathError::Nul),
            ("/etc/hostname", ProjectPathError::Absolute),
            ("\\Windows\\win.ini", ProjectPathError::Absolute),
            ("\\\\server\\share\\x", ProjectPathError::Absolute),
            ("//server/share/x", ProjectPathError::Absolute),
            ("\\\\?\\C:\\x", ProjectPathError::Absolute),
            ("C:\\Windows\\win.ini", ProjectPathError::Prefix),
            ("C:/Windows/win.ini", ProjectPathError::Prefix),
            ("c:x", ProjectPathError::Prefix),
            ("a.txt:stream", ProjectPathError::Prefix),
            ("..", ProjectPathError::ParentDir),
            ("a/../../x", ProjectPathError::ParentDir),
            ("a\\..\\..\\x", ProjectPathError::ParentDir),
            (".. /x", ProjectPathError::ParentDir),
            ("a/.../x", ProjectPathError::ParentDir),
            ("a/. ./x", ProjectPathError::ParentDir),
            ("..\u{20}", ProjectPathError::ParentDir),
            ("a..", ProjectPathError::TrailingDotOrSpace),
            ("a/b. /c", ProjectPathError::TrailingDotOrSpace),
            ("CON", ProjectPathError::Device("CON".into())),
            ("CONIN$", ProjectPathError::Device("CONIN$".into())),
            ("a/conout$.txt", ProjectPathError::Device("conout$.txt".into())),
            ("a/nul.txt", ProjectPathError::Device("nul.txt".into())),
            ("lpt9", ProjectPathError::Device("lpt9".into())),
            (
                "Com1.pgraph.ron",
                ProjectPathError::Device("Com1.pgraph.ron".into()),
            ),
        ];
        for (input, want) in cases {
            assert_eq!(check_relative(input), Err(want), "{input:?}");
        }
    }

    #[test]
    fn lexical_rules_keep_plain_relative_paths() {
        assert_eq!(
            check_relative("graphs/rocks.pgraph.ron"),
            Ok(PathBuf::from("graphs").join("rocks.pgraph.ron"))
        );
        assert_eq!(
            check_relative("./a\\b//c"),
            Ok(PathBuf::from("a").join("b").join("c"))
        );
        // Names that only start like a device or a parent are fine.
        for ok in ["console.txt", "com10", "lpt", "..a", "nullable/x", "aux_rocks"] {
            assert!(check_relative(ok).is_ok(), "{ok}");
        }
        assert_eq!(ProjectPathError::Absolute.code(), "path_absolute");
    }

    #[test]
    fn read_and_write_resolve_inside_the_root() {
        let root = temp_root("inside");
        std::fs::create_dir_all(root.join("g")).expect("dir");
        std::fs::write(root.join("g/a.txt"), "x").expect("file");
        assert_eq!(resolve_read(&root, "g/a.txt"), Ok(root.join("g").join("a.txt")));
        assert!(matches!(
            resolve_read(&root, "g/missing.txt"),
            Err(ProjectPathError::Io(_))
        ));
        // A write may name directories that do not exist yet; nothing is created.
        assert_eq!(
            resolve_write(&root, "new/dir/b.txt"),
            Ok(root.join("new").join("dir").join("b.txt"))
        );
        assert!(!root.join("new").exists());
        assert_eq!(resolve_write(&root, "/tmp/x"), Err(ProjectPathError::Absolute));
        assert_eq!(resolve_read(&root, "C:\\x"), Err(ProjectPathError::Prefix));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn roots_take_relative_and_absolute_paths_inside_them_only() {
        let root = temp_root("roots-project");
        let extra = temp_root("roots-extra");
        let outside = temp_root("roots-outside");
        let roots = [root.clone(), extra.clone()];
        let s = |p: &Path| p.display().to_string();
        assert_eq!(
            resolve_in_roots(&roots, "a/b.txt"),
            Ok(root.join("a").join("b.txt"))
        );
        assert_eq!(
            resolve_in_roots(&roots, &s(&root.join("x.txt"))),
            Ok(root.join("x.txt"))
        );
        assert_eq!(
            resolve_in_roots(&roots, &s(&extra.join("new/y.txt"))),
            Ok(extra.join("new/y.txt"))
        );
        assert_eq!(
            resolve_in_roots(&roots, &s(&outside.join("z.txt"))),
            Err(ProjectPathError::Escapes)
        );
        assert_eq!(
            resolve_in_roots(&roots, &format!("{}/../x", root.display())),
            Err(ProjectPathError::ParentDir)
        );
        assert_eq!(resolve_in_roots(&roots, "C:x"), Err(ProjectPathError::Prefix));
        assert_eq!(resolve_in_roots(&roots, "\\x"), Err(ProjectPathError::Absolute));
        assert_eq!(resolve_in_roots(&roots, "../x"), Err(ProjectPathError::ParentDir));
        #[cfg(unix)]
        {
            assert_eq!(
                resolve_in_roots(&roots, "/etc/hostname"),
                Err(ProjectPathError::Escapes)
            );
            std::os::unix::fs::symlink(&outside, root.join("link")).expect("link");
            assert_eq!(
                resolve_in_roots(&roots, "link/z.txt"),
                Err(ProjectPathError::Escapes)
            );
            assert_eq!(
                resolve_in_roots(&roots, &s(&root.join("link").join("z.txt"))),
                Err(ProjectPathError::Escapes)
            );
        }
        assert!(matches!(resolve_in_roots(&[], "a"), Err(ProjectPathError::Io(_))));
        for d in [root, extra, outside] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_leading_out_are_refused() {
        let root = temp_root("link-root");
        let outside = temp_root("link-outside");
        std::fs::write(outside.join("secret.txt"), "s").expect("secret");
        std::os::unix::fs::symlink(&outside, root.join("out")).expect("dir link");
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("s.txt")).expect("file link");
        std::os::unix::fs::symlink(outside.join("gone"), root.join("dangling")).expect("dangling");
        assert_eq!(
            resolve_read(&root, "out/secret.txt"),
            Err(ProjectPathError::Escapes)
        );
        assert_eq!(resolve_read(&root, "s.txt"), Err(ProjectPathError::Escapes));
        assert_eq!(
            resolve_write(&root, "out/new.txt"),
            Err(ProjectPathError::Escapes)
        );
        assert_eq!(
            resolve_write(&root, "out/deep/new.txt"),
            Err(ProjectPathError::Escapes)
        );
        assert_eq!(resolve_write(&root, "s.txt"), Err(ProjectPathError::Escapes));
        assert_eq!(resolve_write(&root, "dangling"), Err(ProjectPathError::Escapes));
        assert!(!outside.join("deep").exists());
        // A symlink that stays inside the root is fine.
        std::fs::create_dir_all(root.join("real")).expect("real");
        std::os::unix::fs::symlink(root.join("real"), root.join("alias")).expect("inner link");
        assert!(resolve_write(&root, "alias/x.txt").is_ok());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
