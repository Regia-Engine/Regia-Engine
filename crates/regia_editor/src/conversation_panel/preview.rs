// SPDX-License-Identifier: MPL-2.0
//! Preview: step through a conversation from any node, picking choices, with the condition source
//! mocked by a table (ADR-3298).
//!
//! The stepping rules are the runner's (`regia_framework::conversation::runner`, spec 56 § 2.5):
//! a condition missing from the table is unknown, and unknown is false; a `Choice` offers the
//! options whose condition is absent or true and, when none is, takes the first option without a
//! condition or else ends; a run that takes more than `max_steps` non-blocking steps in a row
//! stops (`step_budget`). Lines do not wait for time here: each step call moves past the line.

use std::collections::BTreeMap;

use regia_framework::conversation::{ConversationAsset, Step};

/// Where the preview stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewState {
    /// Showing a line (step index, line id).
    Line {
        /// The `Line` step.
        step: usize,
        /// The line id.
        line: String,
    },
    /// Waiting for a choice.
    Choice {
        /// The `Choice` step.
        step: usize,
        /// The offered options: (index in the step's options, text key, goto label).
        offered: Vec<(usize, String, String)>,
    },
    /// Finished.
    Finished {
        /// `finished` (End or past the last step), `step_budget` or `label_not_found`.
        reason: &'static str,
        /// What went wrong, for the errors.
        detail: Option<String>,
    },
}

impl PreviewState {
    /// The step the preview stands at, if any.
    #[must_use]
    pub fn step(&self) -> Option<usize> {
        match self {
            Self::Line { step, .. } | Self::Choice { step, .. } => Some(*step),
            Self::Finished { .. } => None,
        }
    }
}

/// One preview run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    /// Condition name to value; a missing name is unknown (false).
    pub conditions: BTreeMap<String, bool>,
    /// The current state.
    pub state: PreviewState,
    /// Every step executed, in order.
    pub trace: Vec<usize>,
    /// Events emitted, in order.
    pub events: Vec<String>,
    /// The most non-blocking steps in a row.
    pub max_steps: u32,
}

impl Preview {
    /// Starts at step `start` with `conditions` and runs to the first line, choice or end.
    #[must_use]
    pub fn start(
        asset: &ConversationAsset,
        start: usize,
        conditions: BTreeMap<String, bool>,
        max_steps: u32,
    ) -> Self {
        let mut preview = Self {
            conditions,
            state: PreviewState::Finished {
                reason: "finished",
                detail: None,
            },
            trace: Vec::new(),
            events: Vec::new(),
            max_steps: max_steps.max(1),
        };
        preview.run_from(asset, start);
        preview
    }

    fn eval(&self, name: &str) -> bool {
        self.conditions.get(name).copied().unwrap_or(false)
    }

    /// Moves on: past the shown line, or by taking option `choose` (an index among the
    /// offered options) of the waiting choice.
    ///
    /// # Errors
    ///
    /// `no_choice_pending` (a choice index while no choice waits), `choice_required` (a choice
    /// waits and no index was given), `choice_out_of_range`, `finished`.
    pub fn step(&mut self, asset: &ConversationAsset, choose: Option<usize>) -> Result<(), &'static str> {
        match (&self.state, choose) {
            (PreviewState::Finished { .. }, _) => Err("finished"),
            (PreviewState::Line { .. }, Some(_)) => Err("no_choice_pending"),
            (PreviewState::Line { step, .. }, None) => {
                let next = step + 1;
                self.run_from(asset, next);
                Ok(())
            }
            (PreviewState::Choice { .. }, None) => Err("choice_required"),
            (PreviewState::Choice { offered, .. }, Some(k)) => {
                let Some((_, _, goto)) = offered.get(k).cloned() else {
                    return Err("choice_out_of_range");
                };
                self.jump(asset, &goto);
                Ok(())
            }
        }
    }

    fn jump(&mut self, asset: &ConversationAsset, label: &str) {
        match asset.labels().get(label) {
            Some(&target) => self.run_from(asset, target),
            None => {
                self.state = PreviewState::Finished {
                    reason: "label_not_found",
                    detail: Some(format!("label `{label}` not found")),
                };
            }
        }
    }

    fn run_from(&mut self, asset: &ConversationAsset, start: usize) {
        let steps = asset.effective_steps();
        let labels = asset.labels();
        let mut pc = start;
        let mut taken = 0u32;
        loop {
            let Some(step) = steps.get(pc) else {
                self.state = PreviewState::Finished {
                    reason: "finished",
                    detail: None,
                };
                return;
            };
            if taken >= self.max_steps {
                self.state = PreviewState::Finished {
                    reason: "step_budget",
                    detail: Some(format!(
                        "more than {} non-blocking steps in a row",
                        self.max_steps
                    )),
                };
                return;
            }
            self.trace.push(pc);
            let target = |label: &str| labels.get(label).copied().ok_or_else(|| label.to_string());
            let next = match step {
                Step::Line(id) => {
                    self.state = PreviewState::Line {
                        step: pc,
                        line: id.clone(),
                    };
                    return;
                }
                Step::Label(_) => Ok(pc + 1),
                Step::Event(name) => {
                    self.events.push(name.clone());
                    Ok(pc + 1)
                }
                Step::Goto(label) => target(label),
                Step::If { cond, then, else_ } => target(if self.eval(cond) { then } else { else_ }),
                Step::End => {
                    self.state = PreviewState::Finished {
                        reason: "finished",
                        detail: None,
                    };
                    return;
                }
                Step::Choice { options, .. } => {
                    let offered: Vec<(usize, String, String)> = options
                        .iter()
                        .enumerate()
                        .filter(|(_, o)| o.cond.as_ref().is_none_or(|c| self.eval(c)))
                        .map(|(k, o)| (k, o.text.clone(), o.goto.clone()))
                        .collect();
                    if offered.is_empty() {
                        match options.iter().find(|o| o.cond.is_none()) {
                            Some(first) => target(&first.goto),
                            None => {
                                self.state = PreviewState::Finished {
                                    reason: "finished",
                                    detail: None,
                                };
                                return;
                            }
                        }
                    } else {
                        self.state = PreviewState::Choice { step: pc, offered };
                        return;
                    }
                }
            };
            match next {
                Ok(n) => pc = n,
                Err(label) => {
                    self.state = PreviewState::Finished {
                        reason: "label_not_found",
                        detail: Some(format!("label `{label}` not found")),
                    };
                    return;
                }
            }
            taken += 1;
        }
    }
}
