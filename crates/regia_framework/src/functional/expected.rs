// SPDX-License-Identifier: MPL-2.0
//! The expected-value store behind `TestContext::expect_value` (spec 41 § 2.6): `<level>.expected.ron`,
//! test name + key -> value, sorted, written atomically.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The `.expected.ron` format version.
pub const EXPECTED_FORMAT_VERSION: u32 = 1;

/// Default relative tolerance for floats in expected values.
pub const DEFAULT_RELATIVE_TOLERANCE: f64 = 1e-5;

#[derive(Serialize, Deserialize)]
struct ExpectedFile {
    format_version: u32,
    tests: BTreeMap<String, BTreeMap<String, Value>>,
}

/// The tests of `<level>.expected.ron` text.
fn parse_tests(text: &str) -> Result<BTreeMap<String, BTreeMap<String, Value>>, String> {
    let file: ExpectedFile = ron::from_str(text).map_err(|e| e.to_string())?;
    if file.format_version > EXPECTED_FORMAT_VERSION {
        return Err(format!(
            "format_version {} is newer than this build understands ({EXPECTED_FORMAT_VERSION})",
            file.format_version
        ));
    }
    Ok(file.tests)
}

/// Why the store could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum ExpectedError {
    /// File IO failed.
    #[error("expected-value file {path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// The file did not parse.
    #[error("expected-value file {path}: {message}")]
    Parse {
        /// The file.
        path: PathBuf,
        /// The cause.
        message: String,
    },
}

/// Expected values of one level.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExpectedStore {
    path: Option<PathBuf>,
    tests: BTreeMap<String, BTreeMap<String, Value>>,
    dirty: bool,
}

impl ExpectedStore {
    /// Serde shape fingerprint of the file (for `cargo xtask check-formats`).
    pub fn schema_fingerprint() -> String {
        regia_core::schema::join_field_lists(&[regia_core::schema::serde_field_names::<ExpectedFile>()])
    }

    /// The file beside a level: `levels/arena.level.ron` -> `levels/arena.expected.ron`.
    pub fn path_for_level(project_root: &Path, level: &str) -> PathBuf {
        let stem = level.strip_suffix(".level.ron").unwrap_or(level);
        project_root.join(format!("{stem}.expected.ron"))
    }

    /// A store that is never written (in-memory runs).
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Parses `<level>.expected.ron` text into a store that is never written.
    pub fn from_ron_str(text: &str) -> Result<Self, String> {
        Ok(Self {
            tests: parse_tests(text)?,
            ..Self::default()
        })
    }

    /// Loads `path`; a missing file is an empty store that will be written to `path`.
    pub fn load(path: &Path) -> Result<Self, ExpectedError> {
        let tests = match std::fs::read_to_string(path) {
            Ok(text) => parse_tests(&text).map_err(|message| ExpectedError::Parse {
                path: path.to_path_buf(),
                message,
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(source) => {
                return Err(ExpectedError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        Ok(Self {
            path: Some(path.to_path_buf()),
            tests,
            dirty: false,
        })
    }

    /// The stored value for `test` and `key`.
    pub fn get(&self, test: &str, key: &str) -> Option<&Value> {
        self.tests.get(test)?.get(key)
    }

    /// Stores a value.
    pub fn set(&mut self, test: &str, key: &str, value: Value) {
        self.tests
            .entry(test.to_owned())
            .or_default()
            .insert(key.to_owned(), value);
        self.dirty = true;
    }

    /// Clears every entry of `test`.
    pub fn clear_test(&mut self, test: &str) {
        if self.tests.remove(test).is_some() {
            self.dirty = true;
        }
    }

    /// Writes the file atomically (tmp + rename) when something changed and the store has a path.
    pub fn save(&mut self) -> Result<(), ExpectedError> {
        let Some(path) = self.path.clone().filter(|_| self.dirty) else {
            return Ok(());
        };
        let file = ExpectedFile {
            format_version: EXPECTED_FORMAT_VERSION,
            tests: self.tests.clone(),
        };
        let text = ron::ser::to_string_pretty(&file, regia_core::ron_pretty()).map_err(|e| {
            ExpectedError::Parse {
                path: path.clone(),
                message: e.to_string(),
            }
        })?;
        let io = |source| ExpectedError::Io {
            path: path.clone(),
            source,
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let tmp = path.with_extension("ron.tmp");
        std::fs::write(&tmp, text).map_err(io)?;
        std::fs::rename(&tmp, &path).map_err(io)?;
        self.dirty = false;
        Ok(())
    }
}

/// Compares `actual` with `expected`. A float matches when the absolute difference is at most
/// `relative` times the absolute expected value (spec 41). An expected `0` matches only an
/// actual `0`. Returns one line per difference as `path: expected E, got A`, in document order.
pub fn diff_values(expected: &Value, actual: &Value, relative: f64) -> Vec<String> {
    let mut out = Vec::new();
    walk("", expected, actual, relative, &mut out);
    out
}

fn show(v: &Value) -> String {
    v.to_string()
}

fn walk(path: &str, e: &Value, a: &Value, tol: f64, out: &mut Vec<String>) {
    let here = if path.is_empty() { "$" } else { path };
    match (e, a) {
        (Value::Object(em), Value::Object(am)) => {
            for (k, ev) in em {
                let child = format!("{path}.{k}");
                match am.get(k) {
                    Some(av) => walk(&child, ev, av, tol, out),
                    None => out.push(format!("{child}: expected {}, missing in actual", show(ev))),
                }
            }
            for (k, av) in am {
                if !em.contains_key(k) {
                    out.push(format!("{path}.{k}: unexpected {}", show(av)));
                }
            }
        }
        (Value::Array(ea), Value::Array(aa)) => {
            if ea.len() != aa.len() {
                out.push(format!("{here}: expected {} items, got {}", ea.len(), aa.len()));
            }
            for (i, (ev, av)) in ea.iter().zip(aa).enumerate() {
                walk(&format!("{path}[{i}]"), ev, av, tol, out);
            }
        }
        (Value::Number(en), Value::Number(an)) => {
            let (ef, af) = (en.as_f64().unwrap_or(f64::NAN), an.as_f64().unwrap_or(f64::NAN));
            let both_int = en.is_i64() && an.is_i64() || en.is_u64() && an.is_u64();
            let equal = if both_int {
                en == an
            } else {
                (ef - af).abs() <= tol * ef.abs()
            };
            if !equal {
                out.push(format!("{here}: expected {en}, got {an}"));
            }
        }
        _ => {
            if e != a {
                out.push(format!("{here}: expected {}, got {}", show(e), show(a)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn diff_reports_field_paths_and_honors_tolerance() {
        let e = json!({"pos": [1.0, 2.0], "name": "a", "n": 3});
        let same = json!({"pos": [1.000001, 2.0], "name": "a", "n": 3});
        assert!(diff_values(&e, &same, 1e-5).is_empty());
        let other = json!({"pos": [1.0, 2.5], "name": "b", "n": 4, "extra": true});
        let mut d = diff_values(&e, &other, 1e-5);
        d.sort();
        let mut want: Vec<String> = [
            ".extra: unexpected true",
            ".n: expected 3, got 4",
            ".name: expected \"a\", got \"b\"",
            ".pos[1]: expected 2.0, got 2.5",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        want.sort();
        assert_eq!(d, want);
    }

    /// A value near 1 still matches at 1e-5, and a difference that only fitted the old
    /// absolute floor of 1 does not match a small expected value.
    #[test]
    fn small_magnitudes_use_relative_tolerance() {
        let tol = DEFAULT_RELATIVE_TOLERANCE;
        assert!(diff_values(&json!(0.0), &json!(0.0), tol).is_empty());
        assert!(
            !diff_values(&json!(0.0), &json!(1e-9), tol).is_empty(),
            "an expected zero is an exact match"
        );
        assert!(diff_values(&json!(0.001), &json!(0.001 + 1e-9), tol).is_empty());
        let off = diff_values(&json!(0.001), &json!(0.001 + 5e-6), tol);
        assert_eq!(off.len(), 1, "{off:?}");
        assert!(off[0].contains("expected 0.001"), "{off:?}");
    }

    #[test]
    fn save_is_sorted_atomic_and_round_trips() {
        let dir = regia_test_utils::unique_temp_dir("expected_store");
        let path = ExpectedStore::path_for_level(&dir, "levels/a.level.ron");
        assert_eq!(path, dir.join("levels/a.expected.ron"));
        let mut store = ExpectedStore::load(&path).expect("missing file is empty");
        assert!(store.get("t", "k").is_none());
        store.set("zeta", "b", json!({"x": 1.5, "tags": ["p", "q"]}));
        store.set("alpha", "a", json!(7));
        store.save().expect("saves");
        assert!(!path.with_extension("ron.tmp").exists(), "tmp file renamed away");
        let text = std::fs::read_to_string(&path).expect("reads");
        assert!(
            text.find("alpha").expect("alpha") < text.find("zeta").expect("zeta"),
            "{text}"
        );
        let again = ExpectedStore::load(&path).expect("loads");
        assert_eq!(again.get("alpha", "a"), Some(&json!(7)));
        assert_eq!(
            again.get("zeta", "b"),
            Some(&json!({"x": 1.5, "tags": ["p", "q"]}))
        );
        let mut again = again;
        again.clear_test("alpha");
        assert!(again.get("alpha", "a").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn newer_version_names_the_file() {
        let dir = regia_test_utils::unique_temp_dir("expected_newer");
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("a.expected.ron");
        std::fs::write(&path, "(format_version: 9, tests: {})").expect("write");
        let err = ExpectedStore::load(&path).expect_err("newer");
        assert!(err.to_string().contains("a.expected.ron"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
