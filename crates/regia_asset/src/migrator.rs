// SPDX-License-Identifier: MPL-2.0
//! Project content migrators (`docs/specs/12-assets.md` § 4a).

use std::path::Path;

use thiserror::Error;

/// Result of running all registered migrators.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Migrators that ran successfully.
    pub applied: Vec<String>,
    /// Human-readable notes (no-op steps still listed).
    pub notes: Vec<String>,
}

/// One migrator step for a content format.
pub trait ContentMigrator: Send + Sync {
    /// Stable name (`meta-v1`, `material-v1`, ...).
    fn name(&self) -> &'static str;

    /// Apply migrations in `project_root` (may rewrite files in place).
    fn migrate(&self, project_root: &Path) -> Result<(), MigrationError>;
}

/// Registry used by `regia upgrade-project`.
#[derive(Default)]
pub struct MigratorRegistry {
    migrators: Vec<Box<dyn ContentMigrator>>,
}

impl MigratorRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a migrator (later registrations run after earlier ones).
    pub fn register(&mut self, migrator: Box<dyn ContentMigrator>) {
        self.migrators.push(migrator);
    }

    /// Run every migrator and collect a report.
    pub fn upgrade_project(&self, project_root: &Path) -> Result<MigrationReport, MigrationError> {
        let mut report = MigrationReport::default();
        for m in &self.migrators {
            m.migrate(project_root)?;
            report.applied.push(m.name().to_string());
            report.notes.push(format!("{}: ok", m.name()));
        }
        Ok(report)
    }
}

/// Built-in registry for M4 (v1 formats). The steps only log: they neither validate nor rewrite
/// files. Rewriting a `.material.ron` is `migrate_material_ron_bytes`, which the CLI calls.
pub fn default_registry() -> MigratorRegistry {
    let mut reg = MigratorRegistry::new();
    reg.register(Box::new(NoOpMigrator {
        name: "meta-v1",
        note: "meta format_version 1 is current; nothing to migrate",
    }));
    reg.register(Box::new(NoOpMigrator {
        name: "material-ron-v2",
        note: "material.ron v2 migrates on load via builtin_migrations; upgrade-project rewrites files",
    }));
    reg.register(Box::new(NoOpMigrator {
        name: "cooked-mesh-lod-metric",
        note: "CookedMesh lod_metric migrates on decode in from_postcard; pak bytes are not rewritten",
    }));
    reg
}

struct NoOpMigrator {
    name: &'static str,
    note: &'static str,
}

impl ContentMigrator for NoOpMigrator {
    fn name(&self) -> &'static str {
        self.name
    }

    fn migrate(&self, _project_root: &Path) -> Result<(), MigrationError> {
        tracing::info!(target: "regia_asset", migrator = self.name, "{}", self.note);
        Ok(())
    }
}

/// Migration failure.
#[derive(Debug, Error)]
pub enum MigrationError {
    /// A migrator failed.
    #[error("migrator {name} failed: {message}")]
    Failed {
        /// Migrator name.
        name: String,
        /// Details.
        message: String,
    },

    /// IO error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Records its name into a shared log, optionally failing.
    struct Recorder {
        name: &'static str,
        log: Arc<Mutex<Vec<&'static str>>>,
        fail: bool,
    }

    impl ContentMigrator for Recorder {
        fn name(&self) -> &'static str {
            self.name
        }

        fn migrate(&self, _project_root: &Path) -> Result<(), MigrationError> {
            if let Ok(mut log) = self.log.lock() {
                log.push(self.name);
            }
            if self.fail {
                return Err(MigrationError::Failed {
                    name: self.name.into(),
                    message: "boom".into(),
                });
            }
            Ok(())
        }
    }

    fn recorder(name: &'static str, log: &Arc<Mutex<Vec<&'static str>>>, fail: bool) -> Box<Recorder> {
        Box::new(Recorder {
            name,
            log: Arc::clone(log),
            fail,
        })
    }

    #[test]
    fn migrators_run_in_registration_order_and_are_reported() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut reg = MigratorRegistry::new();
        reg.register(recorder("first", &log, false));
        reg.register(recorder("second", &log, false));
        let report = reg.upgrade_project(Path::new(".")).expect("runs");
        assert_eq!(report.applied, ["first", "second"]);
        assert_eq!(report.notes, ["first: ok", "second: ok"]);
        assert_eq!(*log.lock().expect("log"), ["first", "second"]);
    }

    #[test]
    fn a_failing_migrator_stops_the_chain() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut reg = MigratorRegistry::new();
        reg.register(recorder("ok", &log, false));
        reg.register(recorder("bad", &log, true));
        reg.register(recorder("never", &log, false));
        let err = reg.upgrade_project(Path::new(".")).expect_err("fails");
        assert!(matches!(err, MigrationError::Failed { ref name, .. } if name == "bad"));
        assert_eq!(*log.lock().expect("log"), ["ok", "bad"]);
    }

    #[test]
    fn an_empty_registry_reports_nothing() {
        let report = MigratorRegistry::new()
            .upgrade_project(Path::new("."))
            .expect("runs");
        assert_eq!(report, MigrationReport::default());
    }

    #[test]
    fn the_default_registry_runs_its_no_op_steps_without_touching_the_project() {
        let report = default_registry().upgrade_project(Path::new(".")).expect("runs");
        assert_eq!(report.applied.len(), 3);
        assert_eq!(report.applied[0], "meta-v1");
    }
}
