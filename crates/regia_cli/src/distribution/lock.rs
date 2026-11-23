// SPDX-License-Identifier: MPL-2.0
//! The engine's `Cargo.lock` in a project (spec 94 § 3.2).
//!
//! A project without a lockfile resolves every third-party crate to its newest compatible version,
//! not the versions the engine release was tested with (measured on v0.2.5: a fresh `first_person`
//! project resolved 64 of 660 packages differently, `bevy_*` helper crates included). Seeding the
//! project with the engine's `Cargo.lock` keeps them: Cargo drops the entries the project does not
//! use and adds the project's own crates.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::DistError;

/// The registry and git packages of a lockfile: name → versions.
fn locked_versions(text: &str) -> Option<BTreeMap<String, BTreeSet<String>>> {
    let table: toml::Table = toml::from_str(text).ok()?;
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for package in table.get("package")?.as_array()? {
        if package.get("source").is_none() {
            continue;
        }
        let (Some(name), Some(version)) = (
            package.get("name").and_then(toml::Value::as_str),
            package.get("version").and_then(toml::Value::as_str),
        ) else {
            continue;
        };
        out.entry(name.to_string())
            .or_default()
            .insert(version.to_string());
    }
    Some(out)
}

/// How many third-party package/version rows of the project's lockfile are locked to a version the engine's
/// lockfile does not have, among the packages both name. `None` when either lockfile is missing or
/// unreadable.
pub fn lock_drift(project: &Path, engine_dir: &Path) -> Option<usize> {
    let project_lock = std::fs::read_to_string(project.join("Cargo.lock")).ok()?;
    let engine_lock = std::fs::read_to_string(engine_dir.join("Cargo.lock")).ok()?;
    let project = locked_versions(&project_lock)?;
    let engine = locked_versions(&engine_lock)?;
    let drift = project
        .iter()
        .filter_map(|(name, versions)| Some((versions, engine.get(name)?)))
        .map(|(versions, engine)| versions.difference(engine).count())
        .sum();
    Some(drift)
}

/// Copies `engine_dir/Cargo.lock` to `project/Cargo.lock` when the project has none (atomically).
/// Returns whether it wrote one.
///
/// # Errors
///
/// `io_error` when the copy fails.
pub fn seed_lockfile(project: &Path, engine_dir: &Path) -> Result<bool, DistError> {
    let to = project.join("Cargo.lock");
    let from = engine_dir.join("Cargo.lock");
    if to.exists() || !from.is_file() {
        return Ok(false);
    }
    let bytes = std::fs::read(&from).map_err(|err| DistError::io(&from, &err))?;
    // Named by process: two runs must not write into one staging file.
    let staged = project.join(format!("Cargo.lock.{}.tmp", std::process::id()));
    std::fs::write(&staged, bytes).map_err(|err| DistError::io(&staged, &err))?;
    std::fs::rename(&staged, &to).map_err(|err| DistError::io(&to, &err))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn lock(packages: &[(&str, &str, bool)]) -> String {
        let mut text = String::from("version = 4\n");
        for (name, version, registry) in packages {
            text.push_str(&format!(
                "\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\n"
            ));
            if *registry {
                text.push_str("source = \"registry+https://github.com/rust-lang/crates.io-index\"\n");
            }
        }
        text
    }

    #[test]
    fn drift_counts_shared_packages_at_other_versions() {
        let engine = regia_test_utils::unique_temp_dir("dist_lock_engine");
        let project = regia_test_utils::unique_temp_dir("dist_lock_project");
        std::fs::write(
            engine.join("Cargo.lock"),
            lock(&[
                ("serde", "1.0.200", true),
                ("bevy_ptr", "0.20.0", true),
                ("regia", "0.2.5", false),
            ]),
        )
        .expect("engine lock");
        assert_eq!(lock_drift(&project, &engine), None, "no project lock");
        assert!(seed_lockfile(&project, &engine).expect("seed"));
        assert_eq!(lock_drift(&project, &engine), Some(0));
        assert!(
            !seed_lockfile(&project, &engine).expect("again"),
            "an existing lock is kept"
        );
        std::fs::write(
            project.join("Cargo.lock"),
            lock(&[
                ("serde", "1.0.201", true),
                ("bevy_ptr", "0.20.0", true),
                ("my_game", "0.1.0", false),
                ("rand", "0.9.0", true),
            ]),
        )
        .expect("project lock");
        // serde moved; bevy_ptr matches; rand is the game's own; path packages are not counted.
        assert_eq!(lock_drift(&project, &engine), Some(1));
        assert!(!project.join("Cargo.lock.tmp").exists());
        for dir in [engine, project] {
            std::fs::remove_dir_all(dir).ok();
        }
    }

    #[test]
    fn a_missing_engine_lock_seeds_nothing() {
        let engine = regia_test_utils::unique_temp_dir("dist_lock_none");
        let project = regia_test_utils::unique_temp_dir("dist_lock_none_p");
        assert!(!seed_lockfile(&project, &engine).expect("no engine lock"));
        assert!(!project.join("Cargo.lock").exists());
        for dir in [engine, project] {
            std::fs::remove_dir_all(dir).ok();
        }
    }

    /// Writes fixtures by rename so an overlapping test invocation cannot observe partial TOML.
    fn write_lock(dir: &Path, bytes: &[u8]) {
        let staged = dir.join(format!("Cargo.lock.{}.tmp", std::process::id()));
        std::fs::write(&staged, bytes).expect("stage lockfile");
        std::fs::rename(&staged, dir.join("Cargo.lock")).expect("publish lockfile");
    }

    /// The oracle is built from fixture tuples rather than parsing Cargo.lock, keeping its grouping
    /// and set-difference logic independent of `locked_versions`.
    fn oracle_drift(project: &[(&str, &str, bool)], engine: &[(&str, &str, bool)]) -> usize {
        fn external_sets<'a>(
            packages: &'a [(&'a str, &'a str, bool)],
        ) -> BTreeMap<&'a str, BTreeSet<&'a str>> {
            let mut sets = BTreeMap::new();
            for &(name, version, has_source) in packages {
                if has_source {
                    sets.entry(name).or_insert_with(BTreeSet::new).insert(version);
                }
            }
            sets
        }

        let project = external_sets(project);
        let engine = external_sets(engine);
        project
            .iter()
            .filter_map(|(name, versions)| Some((versions, engine.get(name)?)))
            .map(|(versions, engine_versions)| versions.difference(engine_versions).count())
            .sum()
    }

    fn lock_with_malformed_entries(packages: &[(&str, &str, bool)], extras: &str) -> String {
        format!("{}\n{extras}", lock(packages))
    }

    #[test]
    fn drift_matches_external_name_version_set_oracle_and_is_order_invariant() {
        let engine_packages = [
            ("shared", "1.0.0", true),
            ("multi", "1.0.0", true),
            ("multi", "2.0.0", true),
            ("engine_only", "3.0.0", true),
            ("workspace_engine", "0.2.0", false),
            ("shared", "0.1.0", false),
        ];
        let project_packages = [
            ("shared", "1.0.0", true),
            ("shared", "1.1.0", true),
            ("shared", "1.1.0", true), // duplicate package entry is one set member
            ("multi", "1.0.0", true),
            ("multi", "2.0.0", true),
            ("multi", "3.0.0", true),
            ("project_only", "4.0.0", true),
            ("engine_only", "3.0.0", true),
            ("workspace_project", "0.2.0", false),
        ];
        let expected = oracle_drift(&project_packages, &engine_packages);
        assert_eq!(expected, 2, "fixture oracle sanity");

        let engine = regia_test_utils::unique_temp_dir("dist_lock_oracle_engine");
        let project = regia_test_utils::unique_temp_dir("dist_lock_oracle_project");
        let engine_text = lock_with_malformed_entries(&engine_packages, "");
        let project_text = lock_with_malformed_entries(&project_packages, "");
        write_lock(&engine, engine_text.as_bytes());
        write_lock(&project, project_text.as_bytes());
        let engine_before = std::fs::read(engine.join("Cargo.lock")).expect("engine bytes");
        let project_before = std::fs::read(project.join("Cargo.lock")).expect("project bytes");

        assert_eq!(lock_drift(&project, &engine), Some(expected));
        assert_eq!(
            std::fs::read(engine.join("Cargo.lock")).expect("engine bytes"),
            engine_before
        );
        assert_eq!(
            std::fs::read(project.join("Cargo.lock")).expect("project bytes"),
            project_before
        );

        let mut reversed_project = project_packages;
        reversed_project.reverse();
        let mut reversed_engine = engine_packages;
        reversed_engine.reverse();
        write_lock(
            &project,
            lock_with_malformed_entries(&reversed_project, "").as_bytes(),
        );
        write_lock(
            &engine,
            lock_with_malformed_entries(&reversed_engine, "").as_bytes(),
        );
        let reversed_engine_before = std::fs::read(engine.join("Cargo.lock")).expect("reversed engine bytes");
        let reversed_project_before =
            std::fs::read(project.join("Cargo.lock")).expect("reversed project bytes");
        assert_eq!(lock_drift(&project, &engine), Some(expected));
        assert_eq!(
            std::fs::read(engine.join("Cargo.lock")).expect("engine bytes"),
            reversed_engine_before
        );
        assert_eq!(
            std::fs::read(project.join("Cargo.lock")).expect("project bytes"),
            reversed_project_before
        );

        for dir in [engine, project] {
            std::fs::remove_dir_all(dir).ok();
        }
    }

    #[test]
    fn drift_none_and_zero_distinguish_unreadable_from_no_drift() {
        let engine = regia_test_utils::unique_temp_dir("dist_lock_none_engine");
        let project = regia_test_utils::unique_temp_dir("dist_lock_none_project");
        assert_eq!(lock_drift(&project, &engine), None, "both files missing");

        write_lock(&project, lock(&[("serde", "1.0.0", true)]).as_bytes());
        assert_eq!(lock_drift(&project, &engine), None, "engine file missing");
        write_lock(&engine, b"not = [valid TOML");
        assert_eq!(lock_drift(&project, &engine), None, "invalid engine TOML");
        write_lock(&engine, b"version = 4\n");
        assert_eq!(lock_drift(&project, &engine), None, "package list missing");
        write_lock(&engine, b"version = 4\npackage = \"not-an-array\"\n");
        assert_eq!(
            lock_drift(&project, &engine),
            None,
            "package field has wrong type"
        );

        write_lock(
            &engine,
            lock_with_malformed_entries(
                &[("serde", "1.0.0", true)],
                "\n[[package]]\nversion = \"9.0.0\"\nsource = \"registry+example\"\n\
                 \n[[package]]\nname = 17\nversion = \"9.0.0\"\nsource = \"registry+example\"\n\
                 \n[[package]]\nname = \"serde\"\nsource = \"registry+example\"\n\
                 \n[[package]]\nname = \"serde\"\nversion = 17\nsource = \"registry+example\"\n",
            )
            .as_bytes(),
        );
        assert_eq!(
            lock_drift(&project, &engine),
            Some(0),
            "matching valid sets, malformed records skipped"
        );

        write_lock(&project, b"invalid TOML [[");
        assert_eq!(lock_drift(&project, &engine), None, "invalid project TOML");
        for dir in [engine, project] {
            std::fs::remove_dir_all(dir).ok();
        }
    }

    #[test]
    fn seed_lockfile_preserves_existing_project_bytes_and_copies_absent_lock_exactly() {
        let engine = regia_test_utils::unique_temp_dir("dist_lock_seed_contract_engine");
        let project = regia_test_utils::unique_temp_dir("dist_lock_seed_contract_project");
        assert!(!seed_lockfile(&project, &engine).expect("missing engine lock is a no-op"));
        assert!(!project.join("Cargo.lock").exists());

        let engine_bytes = b"version = 4\n\n# engine bytes stay exact\n";
        write_lock(&engine, engine_bytes);
        let project_bytes = b"version = 4\n\n# project lock must remain untouched\n";
        write_lock(&project, project_bytes);
        assert!(!seed_lockfile(&project, &engine).expect("existing project lock is untouched"));
        assert_eq!(
            std::fs::read(project.join("Cargo.lock")).expect("existing bytes"),
            project_bytes
        );

        std::fs::remove_file(project.join("Cargo.lock")).expect("remove project lock");
        assert!(seed_lockfile(&project, &engine).expect("absent lock is seeded"));
        assert_eq!(
            std::fs::read(project.join("Cargo.lock")).expect("seeded bytes"),
            engine_bytes
        );
        for dir in [engine, project] {
            std::fs::remove_dir_all(dir).ok();
        }
    }
}
