// SPDX-License-Identifier: MPL-2.0
//! In-level functional tests (`docs/specs/41-functional-tests-and-session-harness.md`).
//!
//! A test is an entity carrying [`FunctionalTest`] placed in a level; the [`Runner`] resource runs the
//! selected tests one at a time at one fixed point of the frame ([`FunctionalTestSet`]), counting only
//! frames. Bodies are Rust ([`TestBody`]) or data ([`script`]); the agent crate (`functest.*`) and
//! `regia test` drive the runner. Dev/test only: a `panic = "abort"` shipping profile cannot recover
//! a panicking body, so shipping builds leave [`FunctionalTestPlugin`] out (spec 41 § 9).

pub mod assert;
pub mod context;
pub mod driver;
pub mod expected;
pub mod report;
pub mod runner;
pub mod script;
pub mod types;
pub mod viewpoint;

pub use assert::Relation;
pub use context::{TestBody, TestContext, TestHooks, TestRng};
pub use driver::{SessionDriver, SessionDriverAppExt, SessionDriverPlugin};
pub use report::{TestRecord, TestReport};
pub use runner::{BodyRegistry, DiscoveredTest, Runner, discover, functional_tick, runner_is_active};
pub use types::{FunctionalTest, RunOptions, RunnerStatus, Selection, TestBodyRef, Verdict};

use regia_app::prelude::*;

/// The system set holding the runner tick: after gameplay systems, before render extract.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct FunctionalTestSet;

/// Installs the [`Runner`], the body registry and the tick system. Nothing is scheduled while no run
/// is active (a run condition).
pub struct FunctionalTestPlugin;

impl Plugin for FunctionalTestPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<FunctionalTest>()
            .register_type::<TestBodyRef>()
            .register_type::<Verdict>()
            .init_resource::<Runner>()
            .init_resource::<BodyRegistry>()
            .add_systems(
                Update,
                functional_tick.run_if(runner_is_active).in_set(FunctionalTestSet),
            );
        viewpoint::install(app);
    }
}

/// `App` extension to register Rust test bodies.
pub trait FunctionalAppExt {
    /// Registers `T` so `FunctionalTest { body: Rust(type_name::<T>()) }` selects it.
    fn register_functional_test<T: TestBody + Default>(&mut self) -> &mut Self;
}

impl FunctionalAppExt for App {
    fn register_functional_test<T: TestBody + Default>(&mut self) -> &mut Self {
        self.world_mut()
            .get_resource_or_insert_with(BodyRegistry::default)
            .register::<T>();
        self
    }
}

#[cfg(test)]
mod tests;

/// Serde shape fingerprints of the frozen functional-test formats, hashed by `cargo xtask check-formats`
/// (`docs/specs/12-assets.md` § 4a). Version 1 of each has no migration steps yet.
pub mod formats {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};

    use super::expected::{EXPECTED_FORMAT_VERSION, ExpectedStore};
    pub use super::report::test_report_migration_sources;
    use super::report::{PassRecord, StepRecord, TEST_REPORT_FORMAT_VERSION, TestRecord, TestReport};
    use super::script::{
        ExpectClause, FUNCTEST_SCRIPT_FORMAT_VERSION, FuncTestScript, ScriptAction, ScriptStep, WaitClause,
    };
    use super::types::{FUNCTIONAL_TEST_FORMAT_VERSION, FunctionalTest, TestBodyRef, Verdict};

    /// Version of the `FunctionalTest` component.
    pub const FUNCTIONAL_TEST_VERSION: u32 = FUNCTIONAL_TEST_FORMAT_VERSION;
    /// Version of `.functest.ron`.
    pub const FUNCTEST_SCRIPT_VERSION: u32 = FUNCTEST_SCRIPT_FORMAT_VERSION;
    /// Version of the `TestReport` JSON.
    pub const TEST_REPORT_VERSION: u32 = TEST_REPORT_FORMAT_VERSION;
    /// Version of `<level>.expected.ron`.
    pub const EXPECTED_VALUES_VERSION: u32 = EXPECTED_FORMAT_VERSION;

    /// Fingerprint of the `FunctionalTest` component.
    #[must_use]
    pub fn functional_test_fingerprint() -> String {
        join_field_lists(&[
            serde_field_names::<FunctionalTest>(),
            serde_variant_names::<TestBodyRef>(),
            serde_variant_names::<Verdict>(),
        ])
    }

    /// Fingerprint of `.functest.ron`.
    #[must_use]
    pub fn functest_script_fingerprint() -> String {
        join_field_lists(&[
            serde_field_names::<FuncTestScript>(),
            serde_field_names::<ScriptStep>(),
            serde_field_names::<WaitClause>(),
            serde_field_names::<ExpectClause>(),
            serde_variant_names::<ScriptAction>(),
        ])
    }

    /// Fingerprint of the `TestReport` JSON.
    #[must_use]
    pub fn test_report_fingerprint() -> String {
        join_field_lists(&[
            serde_field_names::<TestReport>(),
            serde_field_names::<TestRecord>(),
            serde_field_names::<PassRecord>(),
            serde_field_names::<StepRecord>(),
        ])
    }

    /// Fingerprint of `<level>.expected.ron`.
    #[must_use]
    pub fn expected_values_fingerprint() -> String {
        ExpectedStore::schema_fingerprint()
    }
}
