// SPDX-License-Identifier: MPL-2.0
//! [`VfsPath`]: a validated `mount://relative/path` (`docs/specs/11-platform-and-input.md` § 4).

use std::fmt;
use std::str::FromStr;

/// A VFS mount point: the part of a [`VfsPath`] before `://`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MountPoint {
    /// `assets://`: cooked game content (read-only).
    Assets,
    /// `config://`: shipped configuration (read-only).
    Config,
    /// `user://`: saves and settings (writable).
    User,
    /// `cache://`: data the OS may delete (writable).
    Cache,
    /// `logs://`: session logs, `user://logs` (read-only through the VFS; the logger writes it).
    Logs,
}

impl MountPoint {
    /// Every mount point.
    pub const ALL: [MountPoint; 5] = [
        MountPoint::Assets,
        MountPoint::Config,
        MountPoint::User,
        MountPoint::Cache,
        MountPoint::Logs,
    ];

    /// The scheme name, e.g. `"assets"`.
    pub fn as_str(self) -> &'static str {
        match self {
            MountPoint::Assets => "assets",
            MountPoint::Config => "config",
            MountPoint::User => "user",
            MountPoint::Cache => "cache",
            MountPoint::Logs => "logs",
        }
    }

    /// Whether [`crate::Vfs::write_atomic`] may write here (`user://` and `cache://` only).
    pub fn is_writable(self) -> bool {
        matches!(self, MountPoint::User | MountPoint::Cache)
    }

    pub(crate) fn index(self) -> usize {
        self as usize
    }
}

impl fmt::Display for MountPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MountPoint {
    type Err = VfsPathError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        MountPoint::ALL
            .into_iter()
            .find(|m| m.as_str() == s)
            .ok_or_else(|| VfsPathError::UnknownMount(s.to_owned()))
    }
}

/// Why a string is not a valid [`VfsPath`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VfsPathError {
    /// No `://` separator.
    #[error("missing `mount://` prefix")]
    MissingScheme,
    /// The mount name is not one of [`MountPoint::ALL`].
    #[error("unknown mount `{0}` (expected assets, config, user, cache or logs)")]
    UnknownMount(String),
    /// The relative part starts with `/` (an absolute path).
    #[error("path must be relative to the mount (no leading `/`)")]
    Absolute,
    /// `//`, or a trailing `/`.
    #[error("empty path segment")]
    EmptySegment,
    /// A `..` segment.
    #[error("`..` segments are not allowed")]
    ParentSegment,
    /// A backslash; VFS paths always use `/`.
    #[error("backslashes are not allowed; use `/`")]
    Backslash,
    /// A character that is not allowed in any segment (NUL, control characters, `:`, and the Windows-reserved `< > " | ? *`).
    #[error("invalid character {0:?}")]
    InvalidChar(char),
    /// A segment whose stem is a Windows reserved device name (`NUL`, `COM1`, `CONOUT$`, ...), with or
    /// without an extension; rejected on every platform so a save path is portable.
    #[error("`{0}` is a reserved device name")]
    ReservedName(String),
    /// A segment that ends with `.` or a space, which Windows strips (`slot.sav.` is `slot.sav`).
    #[error("segment `{0}` ends with a dot or a space")]
    TrailingDotOrSpace(String),
}

/// Whether `segment` (checked case-insensitively, ignoring any extension) names a Windows device
/// (`CON`, `NUL`, `COM1`, `CONOUT$`, ...). Such a file name cannot be created on Windows with any
/// extension, so portable names avoid it on every platform.
pub fn is_reserved_device_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or(segment).trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => {
            let bytes = upper.as_bytes();
            bytes.len() == 4
                && (upper.starts_with("COM") || upper.starts_with("LPT"))
                && (b'1'..=b'9').contains(&bytes[3])
        }
    }
}

/// A validated VFS path: a [`MountPoint`] plus a normalised relative path.
///
/// Syntax: `mount://seg/seg/...`. Segments are separated by `/` and may not be empty, `..`, or contain
/// `\`, `:`, NUL or control characters; `.` segments are dropped. `mount://` (empty relative part) is
/// the mount root.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VfsPath {
    mount: MountPoint,
    relative: String,
}

impl VfsPath {
    /// Parses `mount://relative/path`.
    ///
    /// ```
    /// use regia_platform::{MountPoint, VfsPath};
    ///
    /// let path = VfsPath::parse("assets://textures/./rock.ktx2")?;
    /// assert_eq!(path.mount(), MountPoint::Assets);
    /// assert_eq!(path.relative(), "textures/rock.ktx2");
    /// assert_eq!(path.file_name(), Some("rock.ktx2"));
    /// // `..` is refused, so a path cannot leave its mount.
    /// assert!(VfsPath::parse("user://../etc/passwd").is_err());
    /// # Ok::<(), regia_platform::VfsPathError>(())
    /// ```
    pub fn parse(s: &str) -> Result<Self, VfsPathError> {
        let (mount, rest) = s.split_once("://").ok_or(VfsPathError::MissingScheme)?;
        let mount = mount.parse::<MountPoint>()?;
        Ok(Self {
            mount,
            relative: normalise(rest)?,
        })
    }

    /// Builds a path from a mount and a relative path (validated like [`VfsPath::parse`]).
    pub fn new(mount: MountPoint, relative: &str) -> Result<Self, VfsPathError> {
        Ok(Self {
            mount,
            relative: normalise(relative)?,
        })
    }

    /// The root of `mount` (`mount://`).
    pub fn root(mount: MountPoint) -> Self {
        Self {
            mount,
            relative: String::new(),
        }
    }

    /// The mount point.
    pub fn mount(&self) -> MountPoint {
        self.mount
    }

    /// The normalised relative path (no leading or trailing `/`; empty for the root).
    pub fn relative(&self) -> &str {
        &self.relative
    }

    /// True for `mount://`.
    pub fn is_root(&self) -> bool {
        self.relative.is_empty()
    }

    /// Appends a relative path (one or more segments, validated).
    ///
    /// ```
    /// use regia_platform::{MountPoint, VfsPath};
    ///
    /// let saves = VfsPath::root(MountPoint::User).join("saves")?;
    /// let slot = saves.join("slot1.sav")?;
    /// assert_eq!(slot.to_string(), "user://saves/slot1.sav");
    /// assert_eq!(slot.parent(), Some(saves));
    /// # Ok::<(), regia_platform::VfsPathError>(())
    /// ```
    pub fn join(&self, relative: &str) -> Result<Self, VfsPathError> {
        let tail = normalise(relative)?;
        let relative = match (self.relative.is_empty(), tail.is_empty()) {
            (_, true) => self.relative.clone(),
            (true, false) => tail,
            (false, false) => format!("{}/{tail}", self.relative),
        };
        Ok(Self {
            mount: self.mount,
            relative,
        })
    }

    /// The last segment, or `None` for the root.
    pub fn file_name(&self) -> Option<&str> {
        if self.relative.is_empty() {
            None
        } else {
            self.relative.rsplit('/').next()
        }
    }

    /// The containing directory, or `None` for the root.
    pub fn parent(&self) -> Option<Self> {
        if self.relative.is_empty() {
            return None;
        }
        let relative = match self.relative.rsplit_once('/') {
            Some((parent, _)) => parent.to_owned(),
            None => String::new(),
        };
        Some(Self {
            mount: self.mount,
            relative,
        })
    }

    /// The segments of the relative path.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.relative.split('/').filter(|s| !s.is_empty())
    }
}

/// Validates and normalises the relative part of a VFS path.
fn normalise(relative: &str) -> Result<String, VfsPathError> {
    if relative.is_empty() {
        return Ok(String::new());
    }
    if relative.starts_with('/') {
        return Err(VfsPathError::Absolute);
    }
    let mut out = String::with_capacity(relative.len());
    for segment in relative.split('/') {
        match segment {
            "" => return Err(VfsPathError::EmptySegment),
            ".." => return Err(VfsPathError::ParentSegment),
            "." => continue,
            _ => {}
        }
        if segment.contains('\\') {
            return Err(VfsPathError::Backslash);
        }
        if let Some(c) = segment
            .chars()
            .find(|c| c.is_control() || matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*'))
        {
            return Err(VfsPathError::InvalidChar(c));
        }
        if segment.ends_with(['.', ' ']) {
            return Err(VfsPathError::TrailingDotOrSpace(segment.to_owned()));
        }
        if is_reserved_device_name(segment) {
            return Err(VfsPathError::ReservedName(segment.to_owned()));
        }
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(segment);
    }
    Ok(out)
}

impl fmt::Display for VfsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}", self.mount, self.relative)
    }
}

impl FromStr for VfsPath {
    type Err = VfsPathError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        VfsPath::parse(s)
    }
}

impl TryFrom<&str> for VfsPath {
    type Error = VfsPathError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        VfsPath::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<VfsPath, VfsPathError> {
        VfsPath::parse(s)
    }

    #[test]
    fn parses_every_mount() {
        for m in MountPoint::ALL {
            let path = p(&format!("{m}://a/b.txt")).expect("valid");
            assert_eq!(path.mount(), m);
            assert_eq!(path.relative(), "a/b.txt");
            assert_eq!(path.to_string(), format!("{m}://a/b.txt"));
        }
    }

    #[test]
    fn root_and_dot_segments() {
        assert!(p("assets://").expect("root").is_root());
        assert_eq!(
            p("user://./saves/./slot1.sav").expect("valid").relative(),
            "saves/slot1.sav"
        );
        assert!(p("cache://.").expect("dot is root").is_root());
    }

    #[test]
    fn rejects_bad_paths() {
        use VfsPathError::*;
        assert_eq!(p("assets"), Err(MissingScheme));
        assert_eq!(p("textures/a.png"), Err(MissingScheme));
        assert_eq!(p("nope://a"), Err(UnknownMount("nope".into())));
        assert_eq!(p("Assets://a"), Err(UnknownMount("Assets".into())));
        assert_eq!(p("assets:///etc/passwd"), Err(Absolute));
        assert_eq!(p("assets://a//b"), Err(EmptySegment));
        assert_eq!(p("assets://a/"), Err(EmptySegment));
        assert_eq!(p("assets://../secret"), Err(ParentSegment));
        assert_eq!(p("assets://a/../../b"), Err(ParentSegment));
        assert_eq!(p("assets://a\\b"), Err(Backslash));
        assert_eq!(p("assets://C:/Windows"), Err(InvalidChar(':')));
        assert_eq!(p("assets://a\0b"), Err(InvalidChar('\0')));
        assert_eq!(p("assets://a\nb"), Err(InvalidChar('\n')));
    }

    #[test]
    fn rejects_windows_device_names_and_trailing_dots() {
        use VfsPathError::*;
        for name in [
            "NUL.txt", "nul", "Com1.sav", "LPT9", "aux.x.y", "CON", "prn.", "CONOUT$",
        ] {
            let r = p(&format!("user://saves/{name}"));
            assert!(
                matches!(r, Err(ReservedName(_)) | Err(TrailingDotOrSpace(_))),
                "{name}: {r:?}"
            );
        }
        assert_eq!(p("user://NUL.txt"), Err(ReservedName("NUL.txt".into())));
        assert_eq!(p("user://COM1.sav"), Err(ReservedName("COM1.sav".into())));
        assert_eq!(p("user://slot.sav."), Err(TrailingDotOrSpace("slot.sav.".into())));
        assert_eq!(p("user://slot "), Err(TrailingDotOrSpace("slot ".into())));
        // Near misses stay valid.
        for ok in ["COM0", "COM10", "console.txt", "nulls", "LPT", "a.nul", "com1x"] {
            assert!(p(&format!("user://saves/{ok}")).is_ok(), "{ok}");
        }
    }

    #[test]
    fn rejects_names_windows_cannot_store() {
        use VfsPathError::*;
        for bad in ["a<b", "a>b", "a\"b", "a|b", "a?b", "a*b"] {
            assert!(
                matches!(p(&format!("user://{bad}")), Err(InvalidChar(_))),
                "{bad}"
            );
        }
        assert_eq!(p("user://saves/slot."), Err(TrailingDotOrSpace("slot.".into())));
        assert_eq!(p("user://slot "), Err(TrailingDotOrSpace("slot ".into())));
        assert_eq!(p("user://..."), Err(TrailingDotOrSpace("...".into())));
        for bad in [
            "nul",
            "NUL",
            "Nul.txt",
            "con",
            "aux.tar.gz",
            "PRN",
            "com1",
            "LPT9.log",
            "saves/COM5",
        ] {
            assert!(
                matches!(p(&format!("user://{bad}")), Err(ReservedName(_))),
                "{bad}"
            );
        }
        for ok in [
            "null",
            "console",
            "com0",
            "com10",
            "lpt",
            "a.nul",
            "auxiliary.txt",
            ".hidden",
            "a b",
        ] {
            assert!(p(&format!("user://{ok}")).is_ok(), "{ok}");
        }
    }

    #[test]
    fn join_parent_file_name() {
        let root = VfsPath::root(MountPoint::User);
        let saves = root.join("saves").expect("valid");
        let slot = saves.join("slot1/data.bin").expect("valid");
        assert_eq!(slot.to_string(), "user://saves/slot1/data.bin");
        assert_eq!(slot.file_name(), Some("data.bin"));
        assert_eq!(slot.parent().and_then(|p| p.parent()), Some(saves.clone()));
        assert_eq!(saves.parent(), Some(root.clone()));
        assert_eq!(root.parent(), None);
        assert_eq!(root.file_name(), None);
        assert_eq!(saves.join("").expect("empty join"), saves);
        assert_eq!(saves.join("../x"), Err(VfsPathError::ParentSegment));
        assert_eq!(
            slot.segments().collect::<Vec<_>>(),
            ["saves", "slot1", "data.bin"]
        );
    }

    /// Tiny deterministic generator for the property tests (no proptest dependency needed).
    struct XorShift(u64);
    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items[(self.next() % items.len() as u64) as usize]
        }
    }

    const PIECES: &[&str] = &[
        "a", "b", "tex", "x.png", ".", "..", "", "/", "\\", ":", "\0", "é", "dir.d", "...", "a b", "\t",
    ];

    fn random_relative(rng: &mut XorShift) -> String {
        let len = rng.next() % 8;
        (0..len).map(|_| rng.pick(PIECES)).collect()
    }

    /// Properties over random inputs: a parsed path never contains `..`, `\`, empty segments or a
    /// leading `/`; it re-parses to itself from its `Display`; and parsing is idempotent.
    #[test]
    fn property_parsed_paths_are_normalised_and_round_trip() {
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        let mut accepted = 0;
        for _ in 0..20_000 {
            let mount = MountPoint::ALL[(rng.next() % 5) as usize];
            let raw = format!("{mount}://{}", random_relative(&mut rng));
            let Ok(path) = p(&raw) else { continue };
            accepted += 1;
            let rel = path.relative();
            assert!(!rel.starts_with('/'), "{raw:?} -> {rel:?}");
            assert!(!rel.ends_with('/'), "{raw:?} -> {rel:?}");
            assert!(
                !rel.contains('\\') && !rel.contains(':') && !rel.contains('\0'),
                "{raw:?}"
            );
            for seg in rel.split('/').filter(|_| !rel.is_empty()) {
                assert!(!seg.is_empty() && seg != ".." && seg != ".", "{raw:?} -> {rel:?}");
            }
            let again = p(&path.to_string()).expect("display re-parses");
            assert_eq!(again, path);
            if let Some(parent) = path.parent() {
                let name = path.file_name().expect("non-root has a name");
                assert_eq!(parent.join(name).expect("join"), path);
            }
        }
        assert!(
            accepted > 1000,
            "generator should produce valid paths too ({accepted})"
        );
    }

    /// Any path containing a `..` segment, a backslash or a leading `/` is rejected.
    #[test]
    fn property_escapes_are_always_rejected() {
        let mut rng = XorShift(42);
        for _ in 0..5_000 {
            let a = random_relative(&mut rng);
            let b = random_relative(&mut rng);
            for evil in [
                format!("{a}/../{b}"),
                format!("../{b}"),
                format!("/{a}"),
                format!("{a}\\{b}"),
            ] {
                let raw = format!("assets://{evil}");
                if let Ok(path) = p(&raw) {
                    panic!("{raw:?} accepted as {path}");
                }
            }
        }
    }
}
