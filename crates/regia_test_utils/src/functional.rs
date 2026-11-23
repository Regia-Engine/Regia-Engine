// SPDX-License-Identifier: MPL-2.0
//! Helpers to run the functional tests of a level from `cargo test` (spec 41 § 3, `regia_test_utils::functional`).
//!
//! The caller builds the `App` with the game's plugins (so the level's components are registered);
//! [`run_level`] adds the runner when missing, opens the level, runs the selected tests in process and
//! returns the report. Counted frames only; no clocks.

use std::path::Path;

use regia_app::prelude::*;
use regia_framework::functional::{
    FunctionalTestPlugin, RunOptions, Runner, RunnerStatus, Selection, TestReport,
};
use regia_framework::start_level::{StartLevel, add_start_level};

/// Why a level run could not produce a report.
#[derive(Debug, thiserror::Error)]
pub enum TestError {
    /// The runner did not finish within the frame budget.
    #[error("the functional tests did not finish within {0} frames")]
    NotFinished(u32),
    /// The level file is missing.
    #[error("level file {0} does not exist")]
    NoLevel(String),
}

/// Opens `level` (a `.level.ron` under `project`) in `app` and runs the tests that match `selection`.
/// `max_frames` bounds the run (a frame count, not a time). `options.level` and
/// `options.project_root` are replaced by `level` and `project`.
pub fn run_level(
    app: &mut App,
    project: &Path,
    level: &str,
    selection: &Selection,
    options: RunOptions,
    max_frames: u32,
) -> Result<TestReport, TestError> {
    let path = project.join(level);
    if !path.is_file() {
        return Err(TestError::NoLevel(path.display().to_string()));
    }
    if app.world().get_resource::<Runner>().is_none() {
        app.add_plugins(FunctionalTestPlugin);
    }
    add_start_level(app, StartLevel::new(path));
    // One frame opens the level; the tests are discovered on the next tick.
    app.update();
    let options = RunOptions {
        level: level.to_owned(),
        project_root: Some(project.to_path_buf()),
        ..options
    };
    app.world_mut()
        .resource_mut::<Runner>()
        .start(selection.clone(), options);
    for _ in 0..max_frames {
        app.update();
        let runner = app.world().resource::<Runner>();
        if runner.status() == RunnerStatus::Done {
            return Ok(runner.report().clone());
        }
    }
    Err(TestError::NotFinished(max_frames))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::functional::{
        FunctionalAppExt, TestBody, TestContext, Verdict, report::EXIT_FAILED, report::EXIT_PASSED,
    };
    use regia_scene::ScenePlugin;

    #[derive(Default)]
    struct Good;
    impl TestBody for Good {
        fn begin(&mut self, cx: &mut TestContext) {
            cx.begin_step("math");
            cx.equal(&(2 + 2), &4, "sum");
            cx.end_step();
            cx.conclude(Verdict::Passed, "ok");
        }
    }

    #[derive(Default)]
    struct Bad;
    impl TestBody for Bad {
        fn begin(&mut self, cx: &mut TestContext) {
            cx.begin_step("wrong math");
            cx.equal(&(2 + 2), &5, "sum");
            cx.end_step();
            cx.conclude(Verdict::Passed, "claimed ok");
        }
    }

    fn level(good_a: &str, good_b: &str, bad: Option<&str>) -> String {
        let entity = |guid: &str, name: &str, body: &str| {
            format!(
                r#"(guid: "{guid}", name: "{name}", components: {{
                    "regia_framework::functional::FunctionalTest": (format_version: 1, enabled: true, description: "", owner: "", tags: ["smoke"],
                        frame_limit: 20, ready_limit: 5, on_limit: Failed, viewpoint: None, settings: [],
                        body: Rust("{body}"), reset_expected: false),
                }}),"#
            )
        };
        let mut entities = entity("8b6f2a11-0000-4000-8000-000000000001", "a_good", good_a);
        entities += &entity("8b6f2a11-0000-4000-8000-000000000002", "b_good", good_b);
        if let Some(bad) = bad {
            entities += &entity("8b6f2a11-0000-4000-8000-000000000003", "c_bad", bad);
        }
        format!("(format_version: 1, entities: [{entities}])")
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ScenePlugin, FunctionalTestPlugin));
        app.register_functional_test::<Good>()
            .register_functional_test::<Bad>();
        app
    }

    #[test]
    fn a_level_with_two_passing_tests_and_one_failing_exits_1_naming_the_step() {
        let dir = crate::unique_temp_dir("functest_level");
        std::fs::create_dir_all(dir.join("levels")).expect("dir");
        let good = std::any::type_name::<Good>();
        let bad = std::any::type_name::<Bad>();
        std::fs::write(dir.join("levels/arena.level.ron"), level(good, good, Some(bad))).expect("level");
        let mut app = app();
        let report = run_level(
            &mut app,
            &dir,
            "levels/arena.level.ron",
            &Selection::default(),
            RunOptions::default(),
            200,
        )
        .expect("runs");
        let verdicts: Vec<_> = report
            .tests
            .iter()
            .map(|t| (t.name.as_str(), t.verdict))
            .collect();
        assert_eq!(
            verdicts,
            vec![
                ("a_good", Verdict::Passed),
                ("b_good", Verdict::Passed),
                ("c_bad", Verdict::Failed)
            ]
        );
        let failing = &report.tests[2];
        assert_eq!(failing.steps[0].name, "wrong math");
        assert!(
            failing.message.contains("[wrong math] sum: expected 5, got 4"),
            "{}",
            failing.message
        );
        assert_eq!(report.exit_code(), EXIT_FAILED);
        let xml = report.to_junit();
        assert!(xml.contains("failures=\"1\""), "{xml}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_passing_tests_exit_0_and_a_missing_level_is_an_error() {
        let dir = crate::unique_temp_dir("functest_level_ok");
        std::fs::create_dir_all(dir.join("levels")).expect("dir");
        let good = std::any::type_name::<Good>();
        std::fs::write(dir.join("levels/arena.level.ron"), level(good, good, None)).expect("level");
        let mut app = app();
        let report = run_level(
            &mut app,
            &dir,
            "levels/arena.level.ron",
            &Selection::default(),
            RunOptions::default(),
            200,
        )
        .expect("runs");
        assert_eq!(report.exit_code(), EXIT_PASSED);
        let mut app = self::app();
        let err = run_level(
            &mut app,
            &dir,
            "levels/none.level.ron",
            &Selection::default(),
            RunOptions::default(),
            10,
        )
        .expect_err("missing");
        assert!(matches!(err, TestError::NoLevel(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
