// SPDX-License-Identifier: MPL-2.0
//! `.functest.ron` scripts: the data form of a test body (spec 41 § 2.3).
//!
//! A script reuses the agent protocol's condition grammar (`play.expect` / `play.wait_until`); the
//! condition is carried as an opaque JSON value and evaluated by the installed condition service.
//! Format notes: `docs/formats/functest-ron.md`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::context::{TestBody, TestContext};
use super::types::Verdict;

/// The `.functest.ron` format version.
pub const FUNCTEST_SCRIPT_FORMAT_VERSION: u32 = 1;

/// One action a step runs first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ScriptAction {
    /// Injects an input action value for a number of frames.
    Inject {
        /// Action name.
        action: String,
        /// Value (x, y).
        value: (f32, f32),
        /// Frames the injection lasts.
        frames: u32,
    },
}

/// A `wait_until` clause.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WaitClause {
    /// The condition (`play.wait_until` grammar).
    pub condition: Value,
    /// Frames the wait may take before the pass fails.
    pub within_frames: u32,
}

/// One named assertion of a step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExpectClause {
    /// Assertion name for the report.
    pub name: String,
    /// The condition (`play.expect` grammar).
    pub condition: Value,
}

/// One step of a script.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScriptStep {
    /// Step name for the report.
    pub name: String,
    /// Actions run first.
    #[serde(rename = "do", default)]
    pub actions: Vec<ScriptAction>,
    /// Optional wait.
    #[serde(default)]
    pub wait_until: Option<WaitClause>,
    /// Assertions evaluated together in one frame.
    #[serde(default)]
    pub expect: Vec<ExpectClause>,
}

/// A parsed `.functest.ron`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FuncTestScript {
    /// Format version (`1`).
    pub format_version: u32,
    /// Steps in order.
    pub steps: Vec<ScriptStep>,
}

impl FuncTestScript {
    /// Parses script text; a newer `format_version` or an empty script is an error.
    pub fn parse(text: &str) -> Result<Self, String> {
        // The file may be wrapped in a `FuncTestScript( ... )` name.
        let script: FuncTestScript = ron::from_str(text).map_err(|e| e.to_string())?;
        if script.format_version > FUNCTEST_SCRIPT_FORMAT_VERSION {
            return Err(format!(
                "functest script format_version {} is newer than this build understands ({})",
                script.format_version, FUNCTEST_SCRIPT_FORMAT_VERSION
            ));
        }
        if script.steps.is_empty() {
            return Err("script has no steps".to_owned());
        }
        Ok(script)
    }
}

/// Runs a [`FuncTestScript`] as a [`TestBody`].
pub struct ScriptBody {
    script: FuncTestScript,
    index: usize,
    entered: bool,
    wait_frames: u32,
    /// Failures recorded before the current step began: anything above it (a failed `do` action
    /// included) fails the pass at the end of the step (spec 41 § 2.3).
    failures_at_entry: usize,
}

impl ScriptBody {
    /// A body for `script`.
    pub fn new(script: FuncTestScript) -> Self {
        Self {
            script,
            index: 0,
            entered: false,
            wait_frames: 0,
            failures_at_entry: 0,
        }
    }

    fn reset(&mut self) {
        self.index = 0;
        self.entered = false;
        self.wait_frames = 0;
        self.failures_at_entry = 0;
    }

    /// Advances the script by one frame.
    fn advance(&mut self, cx: &mut TestContext) {
        loop {
            let Some(step) = self.script.steps.get(self.index).cloned() else {
                cx.end_step();
                cx.conclude(Verdict::Passed, "all steps passed");
                return;
            };
            if !self.entered {
                self.entered = true;
                self.wait_frames = 0;
                self.failures_at_entry = cx.failure_count();
                cx.begin_step(&step.name);
                for action in &step.actions {
                    let ScriptAction::Inject {
                        action,
                        value,
                        frames,
                    } = action;
                    if let Err(e) = cx.inject(action, [value.0, value.1], *frames) {
                        cx.fail(&format!("inject `{action}`: {e}"));
                    }
                }
                // Actions take effect on the next frame; a step with a wait or expectations
                // re-enters below on that frame.
                if !step.actions.is_empty() {
                    return;
                }
            }
            if let Some(wait) = &step.wait_until {
                match cx.eval_condition(&wait.condition) {
                    Ok((true, _)) => {}
                    Ok((false, detail)) => {
                        self.wait_frames += 1;
                        if self.wait_frames > wait.within_frames {
                            cx.fail(&format!(
                                "wait_until not satisfied within {} frames: {detail}",
                                wait.within_frames
                            ));
                            self.finish_failed(cx, &step.name);
                        }
                        return;
                    }
                    Err(e) => {
                        cx.fail(&format!("wait_until could not be evaluated: {e}"));
                        self.finish_failed(cx, &step.name);
                        return;
                    }
                }
            }
            for clause in &step.expect {
                match cx.eval_condition(&clause.condition) {
                    Ok((true, _)) => {}
                    Ok((false, detail)) => cx.fail(&format!("expect `{}` failed: {detail}", clause.name)),
                    Err(e) => cx.fail(&format!("expect `{}` could not be evaluated: {e}", clause.name)),
                }
            }
            cx.end_step();
            if cx.failure_count() > self.failures_at_entry {
                self.finish_failed(cx, &step.name);
                return;
            }
            self.index += 1;
            self.entered = false;
            // The next step starts in the same frame; steps with actions return above, so a script of
            // N plain steps costs one frame.
        }
    }

    fn finish_failed(&mut self, cx: &mut TestContext, step: &str) {
        cx.end_step();
        cx.conclude(Verdict::Failed, &format!("step `{step}` failed"));
    }
}

impl TestBody for ScriptBody {
    fn begin(&mut self, cx: &mut TestContext) {
        self.reset();
        self.advance(cx);
    }

    fn tick(&mut self, cx: &mut TestContext) {
        if !cx.is_concluded() {
            self.advance(cx);
        }
    }

    fn conclude(&mut self, _cx: &mut TestContext, _verdict: Verdict) {
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
FuncTestScript(
    format_version: 1,
    steps: [
        (name: "walk to the door", do: [Inject(action: "Move", value: (0.0, 1.0), frames: 90)]),
        (name: "door opens", wait_until: Some((condition: (entity: (name: "Door"), component: "Door", path: "state", op: "eq", value: "Open"), within_frames: 120))),
        (name: "player is through", expect: [(name: "x", condition: (resource: "R", path: "n", op: "gt", value: 1))]),
    ],
)
"#;

    #[test]
    fn parses_the_spec_example() {
        let script = FuncTestScript::parse(SAMPLE).expect("parses");
        assert_eq!(script.steps.len(), 3);
        assert_eq!(script.steps[0].actions.len(), 1);
        let wait = script.steps[1].wait_until.as_ref().expect("wait");
        assert_eq!(wait.within_frames, 120);
        assert_eq!(wait.condition["op"], "eq");
        assert_eq!(script.steps[2].expect[0].condition["value"], 1);
    }

    #[test]
    fn rejects_newer_version_and_empty() {
        assert!(
            FuncTestScript::parse("(format_version: 2, steps: [(name: \"a\")])")
                .expect_err("newer")
                .contains("newer")
        );
        assert!(FuncTestScript::parse("(format_version: 1, steps: [])").is_err());
        assert!(FuncTestScript::parse("not ron").is_err());
    }
}
