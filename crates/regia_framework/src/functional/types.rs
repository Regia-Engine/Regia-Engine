// SPDX-License-Identifier: MPL-2.0
//! Data types of the functional-test runtime: the placed component, verdicts, selections and options
//! (`docs/specs/41-functional-tests-and-session-harness.md` § 2, § 3).

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_scene::spawn::ReflectFormatVersion;
use serde::{Deserialize, Serialize};

/// Maximum passes one run may take (spec 41 § 2.2).
pub const MAX_PASSES: usize = 32;

/// The current `FunctionalTest` component format version.
pub const FUNCTIONAL_TEST_FORMAT_VERSION: u32 = 1;

/// The outcome of a pass or of a whole run.
#[derive(Reflect, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[reflect(Default)]
#[type_path = "regia_framework::functional"]
pub enum Verdict {
    /// Not started yet.
    #[default]
    Pending,
    /// Running.
    Active,
    /// Decided: the check held.
    Passed,
    /// Decided: the check did not hold.
    Failed,
    /// The test could not run to a decision (ready timeout, panic, bad setting, too many passes).
    Aborted,
    /// The pass ended without a verdict being stated.
    Undecided,
}

impl Verdict {
    /// Severity used to pick the worst pass: `Aborted` > `Failed` > `Undecided` > `Passed`.
    fn severity(self) -> u8 {
        match self {
            Verdict::Pending | Verdict::Active => 0,
            Verdict::Passed => 1,
            Verdict::Undecided => 2,
            Verdict::Failed => 3,
            Verdict::Aborted => 4,
        }
    }

    /// The worse of two verdicts.
    pub fn worst(self, other: Verdict) -> Verdict {
        if other.severity() > self.severity() {
            other
        } else {
            self
        }
    }

    /// Whether the verdict is final (the pass or run is over).
    pub fn is_decided(self) -> bool {
        !matches!(self, Verdict::Pending | Verdict::Active)
    }
}

/// How a placed test's body is chosen.
#[derive(Reflect, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[type_path = "regia_framework::functional"]
pub enum TestBodyRef {
    /// A `.functest.ron` script, a project-relative path.
    Script(String),
    /// A Rust body registered with `register_functional_test`, by type path.
    Rust(String),
}

impl Default for TestBodyRef {
    fn default() -> Self {
        TestBodyRef::Rust(String::new())
    }
}

/// A functional test placed in a level (spec 41 § 2.2).
#[derive(Component, Reflect, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[reflect(Component, Default, FormatVersion)]
#[type_path = "regia_framework::functional"]
pub struct FunctionalTest {
    /// Format version of this component (`1`).
    pub format_version: u32,
    /// Disabled tests are listed and skipped.
    pub enabled: bool,
    /// What the test checks, for reports.
    pub description: String,
    /// Who owns the test.
    pub owner: String,
    /// Tags for `--tag` selection and ordering. `isolated` reloads the level first.
    pub tags: Vec<String>,
    /// Fixed-step frames a pass may take after Begin; `0` is no limit.
    pub frame_limit: u32,
    /// Frames the ready gate may take; `0` is no limit.
    pub ready_limit: u32,
    /// Verdict applied when `frame_limit` runs out.
    pub on_limit: Verdict,
    /// Guid (text) of an entity whose pose the local camera takes at Begin.
    pub viewpoint: Option<String>,
    /// Engine settings overridden for the run: `(path, value)`; values parse as bool, int, float, else string.
    pub settings: Vec<(String, String)>,
    /// The body.
    pub body: TestBodyRef,
    /// Clears this test's expected-store entries for the current process and then clears this
    /// live component. It does not rewrite the level file, so a committed `true` clears again
    /// on the next process.
    pub reset_expected: bool,
}

impl regia_scene::spawn::FormatVersion for FunctionalTest {
    const CURRENT: u32 = FUNCTIONAL_TEST_FORMAT_VERSION;
}

impl Default for FunctionalTest {
    fn default() -> Self {
        Self {
            format_version: FUNCTIONAL_TEST_FORMAT_VERSION,
            enabled: true,
            description: String::new(),
            owner: String::new(),
            tags: Vec::new(),
            frame_limit: 1800,
            ready_limit: 600,
            on_limit: Verdict::Failed,
            viewpoint: None,
            settings: Vec::new(),
            body: TestBodyRef::default(),
            reset_expected: false,
        }
    }
}

/// Which tests a run takes: name globs (any-of, `*` and `?`) and tags (all-of). Empty means all.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    /// Name globs; a test matches when any glob matches. Empty matches every name.
    pub name_globs: Vec<String>,
    /// Tags a test must all carry.
    pub tags: Vec<String>,
}

/// Options of one run set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunOptions {
    /// Seed for `TestContext::rng`.
    pub seed: u64,
    /// Write missing/changed expected values instead of failing.
    pub update_expected: bool,
    /// Repeat the set until aborted.
    pub looped: bool,
    /// Parameters reachable by `TestContext::param`.
    pub params: BTreeMap<String, String>,
    /// Level path of the loaded level, used for ordering, reports and the expected-value file.
    pub level: String,
    /// Project root; when set, `<level>.expected.ron` is read and written under it.
    pub project_root: Option<std::path::PathBuf>,
}

/// What the runner is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunnerStatus {
    /// No run started.
    Idle,
    /// A test is running.
    Running {
        /// Test name.
        test: String,
        /// Zero-based pass index.
        pass: usize,
        /// Frames since Begin.
        frame: u64,
    },
    /// Every selected test finished.
    Done,
}

/// Glob match with `*` (any run) and `?` (one char), case sensitive.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

impl Selection {
    /// Whether a test named `name` with `tags` is selected.
    pub fn matches(&self, name: &str, tags: &[String]) -> bool {
        let name_ok = self.name_globs.is_empty() || self.name_globs.iter().any(|g| glob_match(g, name));
        name_ok && self.tags.iter().all(|t| tags.contains(t))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worst_verdict_order() {
        assert_eq!(Verdict::Passed.worst(Verdict::Undecided), Verdict::Undecided);
        assert_eq!(Verdict::Undecided.worst(Verdict::Failed), Verdict::Failed);
        assert_eq!(Verdict::Failed.worst(Verdict::Aborted), Verdict::Aborted);
        assert_eq!(Verdict::Aborted.worst(Verdict::Passed), Verdict::Aborted);
    }

    #[test]
    fn globs() {
        assert!(glob_match("smoke*", "smoke_door"));
        assert!(glob_match("*door", "smoke_door"));
        assert!(glob_match("d?or", "door"));
        assert!(!glob_match("d?or", "dor"));
        assert!(glob_match("*", ""));
        assert!(!glob_match("a*b", "acd"));
    }

    #[test]
    fn selection_all_of_tags_any_of_globs() {
        let sel = Selection {
            name_globs: vec!["a*".into(), "b*".into()],
            tags: vec!["x".into(), "y".into()],
        };
        let tags = vec!["x".to_string(), "y".to_string(), "z".to_string()];
        assert!(sel.matches("beta", &tags));
        assert!(!sel.matches("gamma", &tags));
        assert!(!sel.matches("alpha", &["x".to_string()]));
        assert!(Selection::default().matches("anything", &[]));
    }
}
