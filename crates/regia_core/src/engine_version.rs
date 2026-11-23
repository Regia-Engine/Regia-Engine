// SPDX-License-Identifier: MPL-2.0
//! Engine release versions and the upgrade policy (`docs/specs/94-distribution-upgrade.md` § 2).
//!
//! A Regia Engine version is `MAJOR.MINOR.PATCH`. Its **release line** is `0.MINOR` before 1.0 and
//! `MAJOR` from 1.0 on: Cargo's compatibility rule for caret requirements, so a git or registry pin
//! resolves the same way. A project names the line it targets in `Regia.toml [project] engine`
//! (`"0.2"`, or `"0.2.5"` for at least that patch); [`assess`] compares that requirement with the
//! running engine and returns the status `regia upgrade-project` acts on.
//!
//! Pure and dependency-free, so it builds on every target (the CLI, the editor and the runtime may
//! all report it).

use std::fmt;

/// A Regia Engine version, `MAJOR.MINOR.PATCH`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EngineVersion {
    /// Major version (0 before the sellable 1.0).
    pub major: u32,
    /// Minor version.
    pub minor: u32,
    /// Patch version.
    pub patch: u32,
}

/// The version of this build: the workspace version (`Cargo.toml [workspace.package] version`).
pub const CURRENT: EngineVersion = parse_const(env!("CARGO_PKG_VERSION"));

/// The oldest release line `regia upgrade-project` still migrates from. Every format's migrators
/// chain back to the format's v1, so today that is the first line; a line that retires old
/// migrators raises it (spec 94 § 2, "Support window").
pub const OLDEST_UPGRADABLE_LINE: ReleaseLine = ReleaseLine::pre_1_0(1);

/// Every release line that has shipped or is being built, oldest first. The last entry is the
/// line of [`CURRENT`] (a unit test holds that), so a version bump to a new line adds it here.
pub const RELEASE_LINES: &[ReleaseLine] = &[
    ReleaseLine::pre_1_0(1),
    ReleaseLine::pre_1_0(2),
    ReleaseLine::pre_1_0(5),
    ReleaseLine::pre_1_0(6),
    ReleaseLine::pre_1_0(7),
];

/// Compile-time parse of `MAJOR.MINOR.PATCH` (a `-pre` or `+build` suffix is ignored).
const fn parse_const(text: &str) -> EngineVersion {
    let bytes = text.as_bytes();
    let mut parts = [0u32; 3];
    let mut part = 0usize;
    let mut i = 0usize;
    let mut digits = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'-' || b == b'+' {
            break;
        }
        if b == b'.' {
            assert!(digits > 0 && part < 2, "engine version is not MAJOR.MINOR.PATCH");
            part += 1;
            digits = 0;
        } else {
            assert!(b.is_ascii_digit(), "engine version is not MAJOR.MINOR.PATCH");
            parts[part] = parts[part] * 10 + (b - b'0') as u32;
            digits += 1;
        }
        i += 1;
    }
    assert!(digits > 0 && part == 2, "engine version is not MAJOR.MINOR.PATCH");
    EngineVersion::new(parts[0], parts[1], parts[2])
}

/// Why a version, line or requirement string did not parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineVersionError {
    /// The text that was rejected.
    pub text: String,
    /// What shape was expected.
    pub expected: &'static str,
}

impl fmt::Display for EngineVersionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}` is not {}", self.text, self.expected)
    }
}

impl std::error::Error for EngineVersionError {}

/// Splits `text` into one to three dot-separated decimal numbers (no signs, no empty parts).
fn numbers(text: &str, max: usize) -> Option<Vec<u32>> {
    let text = text.trim();
    let mut out = Vec::new();
    for part in text.split('.') {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) || out.len() == max {
            return None;
        }
        out.push(part.parse().ok()?);
    }
    Some(out)
}

impl EngineVersion {
    /// A version from its three numbers.
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self { major, minor, patch }
    }

    /// The version of this build ([`CURRENT`]).
    pub const fn current() -> Self {
        CURRENT
    }

    /// Parses `MAJOR.MINOR.PATCH` (a leading `v` is accepted, as in a tag name).
    ///
    /// # Errors
    ///
    /// Anything that is not three decimal numbers separated by dots.
    pub fn parse(text: &str) -> Result<Self, EngineVersionError> {
        let body = text.trim().strip_prefix('v').unwrap_or(text.trim());
        match numbers(body, 3).as_deref() {
            Some(&[major, minor, patch]) => Ok(Self::new(major, minor, patch)),
            _ => Err(EngineVersionError {
                text: text.to_string(),
                expected: "a version MAJOR.MINOR.PATCH",
            }),
        }
    }

    /// The release line this version belongs to.
    pub const fn line(&self) -> ReleaseLine {
        if self.major == 0 {
            ReleaseLine::pre_1_0(self.minor)
        } else {
            ReleaseLine::major(self.major)
        }
    }
}

impl fmt::Display for EngineVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A release line: the versions that are compatible with each other. `0.2` before 1.0 (`minor` is
/// `Some`), `1` from 1.0 on (`minor` is `None`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReleaseLine {
    /// Major version.
    pub major: u32,
    /// Minor version for a pre-1.0 line; `None` from 1.0 on.
    pub minor: Option<u32>,
}

impl ReleaseLine {
    /// The pre-1.0 line `0.minor`.
    pub const fn pre_1_0(minor: u32) -> Self {
        Self {
            major: 0,
            minor: Some(minor),
        }
    }

    /// The line of a 1.0-or-later major version.
    pub const fn major(major: u32) -> Self {
        Self { major, minor: None }
    }

    /// Parses `0.N` or `N` (N ≥ 1).
    ///
    /// # Errors
    ///
    /// Any other shape (`0`, `1.2`, `0.2.5`, text).
    pub fn parse(text: &str) -> Result<Self, EngineVersionError> {
        let err = || EngineVersionError {
            text: text.to_string(),
            expected: "a release line (`0.N` before 1.0, `N` after)",
        };
        match numbers(text, 2).as_deref() {
            Some(&[0, minor]) => Ok(Self::pre_1_0(minor)),
            Some(&[major]) if major > 0 => Ok(Self::major(major)),
            _ => Err(err()),
        }
    }
}

impl fmt::Display for ReleaseLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.minor {
            Some(minor) => write!(f, "{}.{minor}", self.major),
            None => write!(f, "{}", self.major),
        }
    }
}

/// A project's `Regia.toml [project] engine` value: the lowest engine version it accepts. Its line
/// is the line it targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineRequirement {
    /// The lowest accepted version (missing parts are 0: `"0.2"` is `0.2.0`).
    pub minimum: EngineVersion,
}

impl EngineRequirement {
    /// Parses `"0.2"`, `"0.2.5"`, `"1"`, `"1.3"` or `"1.3.2"`.
    ///
    /// # Errors
    ///
    /// One to three decimal numbers are required; a bare `"0"` names no line.
    pub fn parse(text: &str) -> Result<Self, EngineVersionError> {
        let err = || EngineVersionError {
            text: text.to_string(),
            expected: "an engine requirement (`0.2`, `0.2.5`, `1`, `1.3`)",
        };
        let minimum = match numbers(text, 3).as_deref() {
            Some(&[major]) if major > 0 => EngineVersion::new(major, 0, 0),
            Some(&[major, minor]) => EngineVersion::new(major, minor, 0),
            Some(&[major, minor, patch]) => EngineVersion::new(major, minor, patch),
            _ => return Err(err()),
        };
        Ok(Self { minimum })
    }

    /// The release line the project targets.
    pub const fn line(&self) -> ReleaseLine {
        self.minimum.line()
    }
}

/// How a project's requirement relates to an engine (spec 94 § 2 table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compatibility {
    /// Same line, and the engine is at least the required version.
    Current,
    /// The project names an older line inside the support window.
    UpgradeNeeded {
        /// The project's line.
        from: ReleaseLine,
        /// The engine's line.
        to: ReleaseLine,
    },
    /// The project names no line (written before the field was kept); treated as
    /// [`OLDEST_UPGRADABLE_LINE`].
    Unpinned {
        /// The engine's line.
        to: ReleaseLine,
    },
    /// The project names a line older than [`OLDEST_UPGRADABLE_LINE`].
    UpgradeUnsupported {
        /// The project's line.
        from: ReleaseLine,
        /// The oldest line this engine migrates from.
        oldest: ReleaseLine,
    },
    /// The project needs a newer engine (a newer line, or a newer patch of this line).
    EngineTooOld {
        /// What the project requires.
        required: EngineVersion,
    },
}

impl Compatibility {
    /// The stable status code (lowercase ASCII and `_`; `book/agents/distribution-upgrade.md`).
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::UpgradeNeeded { .. } => "upgrade_needed",
            Self::Unpinned { .. } => "unpinned",
            Self::UpgradeUnsupported { .. } => "upgrade_unsupported",
            Self::EngineTooOld { .. } => "engine_too_old",
        }
    }

    /// True when `regia upgrade-project` may run (it never downgrades and never skips the window).
    pub const fn upgradable(&self) -> bool {
        matches!(
            self,
            Self::Current | Self::UpgradeNeeded { .. } | Self::Unpinned { .. }
        )
    }

    /// The line a project should be stamped with after a successful upgrade, when it changes.
    pub const fn stamp(&self) -> Option<ReleaseLine> {
        match self {
            Self::UpgradeNeeded { to, .. } | Self::Unpinned { to } => Some(*to),
            _ => None,
        }
    }
}

/// Compares a project's requirement with `engine`.
pub fn compatibility(requirement: &EngineRequirement, engine: EngineVersion) -> Compatibility {
    let from = requirement.line();
    let to = engine.line();
    if from > to || (from == to && engine < requirement.minimum) {
        Compatibility::EngineTooOld {
            required: requirement.minimum,
        }
    } else if from == to {
        Compatibility::Current
    } else if from < OLDEST_UPGRADABLE_LINE {
        Compatibility::UpgradeUnsupported {
            from,
            oldest: OLDEST_UPGRADABLE_LINE,
        }
    } else {
        Compatibility::UpgradeNeeded { from, to }
    }
}

/// [`compatibility`] for a requirement that may be missing ([`Compatibility::Unpinned`]).
pub fn assess(requirement: Option<&EngineRequirement>, engine: EngineVersion) -> Compatibility {
    match requirement {
        Some(requirement) => compatibility(requirement, engine),
        None => Compatibility::Unpinned { to: engine.line() },
    }
}

/// The released lines after `from` up to and including `to`, oldest first: the lines whose
/// release notes an upgrade from `from` to `to` must read.
pub fn lines_crossed(from: ReleaseLine, to: ReleaseLine) -> Vec<ReleaseLine> {
    RELEASE_LINES
        .iter()
        .copied()
        .filter(|line| *line > from && *line <= to)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(text: &str) -> EngineRequirement {
        EngineRequirement::parse(text).expect("requirement")
    }

    #[test]
    fn versions_parse_and_display_round_trip() {
        let v = EngineVersion::parse("0.2.5").expect("parse");
        assert_eq!(v, EngineVersion::new(0, 2, 5));
        assert_eq!(v.to_string(), "0.2.5");
        assert_eq!(
            EngineVersion::parse("v1.10.0").expect("tag"),
            EngineVersion::new(1, 10, 0)
        );
        for bad in ["", "0.2", "0.2.5.1", "0.-2.5", "a.b.c", "0..5", "0.2.5 x"] {
            assert!(EngineVersion::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn current_is_the_workspace_version() {
        assert_eq!(
            EngineVersion::parse(env!("CARGO_PKG_VERSION")).expect("pkg version"),
            CURRENT
        );
        assert_eq!(parse_const("1.2.3-dev+abc"), EngineVersion::new(1, 2, 3));
    }

    #[test]
    fn lines_follow_the_cargo_caret_rule() {
        assert_eq!(EngineVersion::new(0, 2, 5).line().to_string(), "0.2");
        assert_eq!(EngineVersion::new(1, 4, 2).line().to_string(), "1");
        assert_eq!(ReleaseLine::parse("0.3").expect("line"), ReleaseLine::pre_1_0(3));
        assert_eq!(ReleaseLine::parse("2").expect("line"), ReleaseLine::major(2));
        for bad in ["0", "1.2", "0.2.5", "x", ""] {
            assert!(ReleaseLine::parse(bad).is_err(), "{bad}");
        }
        // Every pre-1.0 line sorts before 1.
        assert!(ReleaseLine::pre_1_0(99) < ReleaseLine::major(1));
        assert!(ReleaseLine::pre_1_0(1) < ReleaseLine::pre_1_0(2));
    }

    #[test]
    fn requirements_take_one_to_three_numbers() {
        assert_eq!(req("0.2").minimum, EngineVersion::new(0, 2, 0));
        assert_eq!(req("0.2.5").minimum, EngineVersion::new(0, 2, 5));
        assert_eq!(req("1").minimum, EngineVersion::new(1, 0, 0));
        assert_eq!(req("1.3").line(), ReleaseLine::major(1));
        for bad in ["0", "", "0.2.5.1", "^0.2", "latest"] {
            assert!(EngineRequirement::parse(bad).is_err(), "{bad}");
        }
    }

    /// Every row of the spec 94 § 2 table.
    #[test]
    fn compatibility_matches_the_policy_table() {
        let engine = EngineVersion::new(0, 2, 5);
        assert_eq!(compatibility(&req("0.2"), engine), Compatibility::Current);
        assert_eq!(compatibility(&req("0.2.5"), engine), Compatibility::Current);
        assert_eq!(
            compatibility(&req("0.2.6"), engine),
            Compatibility::EngineTooOld {
                required: EngineVersion::new(0, 2, 6)
            }
        );
        assert_eq!(compatibility(&req("0.3"), engine).code(), "engine_too_old");
        assert_eq!(compatibility(&req("1"), engine).code(), "engine_too_old");
        assert_eq!(
            compatibility(&req("0.1"), engine),
            Compatibility::UpgradeNeeded {
                from: ReleaseLine::pre_1_0(1),
                to: ReleaseLine::pre_1_0(2)
            }
        );
        assert_eq!(
            compatibility(&req("0.0.9"), engine),
            Compatibility::UpgradeUnsupported {
                from: ReleaseLine::pre_1_0(0),
                oldest: OLDEST_UPGRADABLE_LINE
            }
        );
        assert_eq!(assess(None, engine).code(), "unpinned");
        assert_eq!(assess(None, engine).stamp(), Some(ReleaseLine::pre_1_0(2)));
        // After 1.0 a newer minor of the same major is the same line.
        assert_eq!(
            compatibility(&req("1.2"), EngineVersion::new(1, 4, 0)),
            Compatibility::Current
        );
        assert_eq!(
            compatibility(&req("1.5"), EngineVersion::new(1, 4, 0)).code(),
            "engine_too_old"
        );
        assert_eq!(
            compatibility(&req("0.2"), EngineVersion::new(1, 0, 0)).code(),
            "upgrade_needed"
        );
    }

    #[test]
    fn only_current_needed_and_unpinned_may_upgrade() {
        let engine = EngineVersion::new(0, 2, 5);
        assert!(compatibility(&req("0.2"), engine).upgradable());
        assert!(compatibility(&req("0.1"), engine).upgradable());
        assert!(assess(None, engine).upgradable());
        assert!(!compatibility(&req("0.3"), engine).upgradable());
        assert!(!compatibility(&req("0.0.1"), engine).upgradable());
        assert_eq!(compatibility(&req("0.2"), engine).stamp(), None);
        assert_eq!(compatibility(&req("0.3"), engine).stamp(), None);
    }

    #[test]
    fn codes_are_lowercase_ascii() {
        let all = [
            Compatibility::Current,
            Compatibility::UpgradeNeeded {
                from: ReleaseLine::pre_1_0(1),
                to: ReleaseLine::pre_1_0(2),
            },
            Compatibility::Unpinned {
                to: ReleaseLine::pre_1_0(2),
            },
            Compatibility::UpgradeUnsupported {
                from: ReleaseLine::pre_1_0(0),
                oldest: OLDEST_UPGRADABLE_LINE,
            },
            Compatibility::EngineTooOld {
                required: EngineVersion::new(9, 0, 0),
            },
        ];
        for c in all {
            assert!(
                c.code()
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{}",
                c.code()
            );
        }
    }

    #[test]
    fn release_lines_are_sorted_and_end_with_this_engine() {
        assert!(RELEASE_LINES.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(RELEASE_LINES.last().copied(), Some(CURRENT.line()));
        assert_eq!(RELEASE_LINES.first().copied(), Some(OLDEST_UPGRADABLE_LINE));
    }

    #[test]
    fn lines_crossed_lists_each_line_after_the_project() {
        let l = ReleaseLine::pre_1_0;
        assert_eq!(lines_crossed(l(1), l(2)), vec![l(2)]);
        assert!(lines_crossed(l(2), l(2)).is_empty());
        assert_eq!(lines_crossed(l(0), l(2)), vec![l(1), l(2)]);
    }
}
