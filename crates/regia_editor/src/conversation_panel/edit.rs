// SPDX-License-Identifier: MPL-2.0
//! Edits of a conversation asset: the operations the panel and `editor.conversation_edit` share
//! (ADR-3298). A batch applies all or nothing to a copy; the caller turns the result into one
//! undo step.
//!
//! Step indices are into the asset's effective steps. An asset that has no `steps` list (every
//! line in file order) gets the explicit list on its first step edit, which plays the same.

use regia_framework::conversation::{ChoiceOption, ConversationAsset, ConversationLine, Step};
use serde::Deserialize;

/// A new step, as the agent writes it (`{ "kind": "goto", "label": "x" }`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NewStep {
    /// `Line(line)`: the line must exist (use `add_line` to create one).
    Line {
        /// The line id.
        line: String,
    },
    /// `Label(name)`.
    Label {
        /// The label name.
        name: String,
    },
    /// `Goto(label)`.
    Goto {
        /// The target label.
        label: String,
    },
    /// `If { cond, then, else }`.
    If {
        /// The condition name.
        cond: String,
        /// The label taken when true.
        then: String,
        /// The label taken when false or unknown.
        #[serde(rename = "else")]
        else_: String,
    },
    /// `Choice { prompt, options }`.
    Choice {
        /// The prompt text key.
        #[serde(default)]
        prompt: Option<String>,
        /// The options.
        #[serde(default)]
        options: Vec<NewOption>,
    },
    /// `Event(name)`.
    Event {
        /// The event name.
        name: String,
    },
    /// `End`.
    End,
}

/// A choice option, as the agent writes it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewOption {
    /// The text key.
    pub text: String,
    /// The condition name.
    #[serde(default)]
    pub cond: Option<String>,
    /// The label to continue at.
    pub goto: String,
}

impl NewOption {
    fn into_option(self) -> ChoiceOption {
        ChoiceOption {
            text: self.text,
            cond: self.cond.filter(|c| !c.is_empty()),
            goto: self.goto,
        }
    }
}

impl NewStep {
    fn into_step(self) -> Step {
        match self {
            Self::Line { line } => Step::Line(line),
            Self::Label { name } => Step::Label(name),
            Self::Goto { label } => Step::Goto(label),
            Self::If { cond, then, else_ } => Step::If { cond, then, else_ },
            Self::Choice { prompt, options } => Step::Choice {
                prompt,
                options: options.into_iter().map(NewOption::into_option).collect(),
            },
            Self::Event { name } => Step::Event(name),
            Self::End => Step::End,
        }
    }
}

/// Which jump of a step to retarget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Branch {
    /// A `Goto`'s label (the default).
    #[default]
    Goto,
    /// An `If`'s `then`.
    Then,
    /// An `If`'s `else`.
    Else,
    /// A choice option (with `option`).
    Option,
}

/// One edit.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConvOp {
    /// Adds a line definition and a `Line` step for it at `at` (default: the end).
    AddLine {
        /// The new line id (unique).
        id: String,
        /// The speaker id.
        speaker: String,
        /// The text key.
        text: String,
        /// Where the step goes.
        #[serde(default)]
        at: Option<usize>,
    },
    /// Inserts a step at `at` (default: the end).
    AddStep {
        /// The step.
        step: NewStep,
        /// Where it goes.
        #[serde(default)]
        at: Option<usize>,
    },
    /// Deletes step `at`. A deleted `Line` step's line is removed too when no other step plays it.
    DeleteStep {
        /// The step index.
        at: usize,
    },
    /// Moves step `from` so it ends up at index `to`.
    MoveStep {
        /// The step index.
        from: usize,
        /// Its new index.
        to: usize,
    },
    /// Changes a line's fields; absent fields stay.
    SetLine {
        /// The line id.
        line: String,
        /// The new text key.
        #[serde(default)]
        text: Option<String>,
        /// The new speaker.
        #[serde(default)]
        speaker: Option<String>,
        /// The new note.
        #[serde(default)]
        note: Option<String>,
        /// The new emotion (empty clears it).
        #[serde(default)]
        emotion: Option<String>,
    },
    /// Changes a choice's prompt (empty clears it).
    SetPrompt {
        /// The `Choice` step.
        at: usize,
        /// The prompt key.
        prompt: String,
    },
    /// Adds an option to the `Choice` step `at`.
    AddOption {
        /// The `Choice` step.
        at: usize,
        /// The text key.
        text: String,
        /// The condition name.
        #[serde(default)]
        cond: Option<String>,
        /// The label to continue at.
        goto: String,
    },
    /// Changes an option; absent fields stay (an empty `cond` clears the condition).
    SetOption {
        /// The `Choice` step.
        at: usize,
        /// The option index.
        option: usize,
        /// The text key.
        #[serde(default)]
        text: Option<String>,
        /// The condition.
        #[serde(default)]
        cond: Option<String>,
        /// The target label.
        #[serde(default)]
        goto: Option<String>,
    },
    /// Deletes an option.
    DeleteOption {
        /// The `Choice` step.
        at: usize,
        /// The option index.
        option: usize,
    },
    /// Moves an option inside its choice.
    MoveOption {
        /// The `Choice` step.
        at: usize,
        /// The option index.
        from: usize,
        /// Its new index.
        to: usize,
    },
    /// Changes an `If`'s condition.
    SetCond {
        /// The `If` step.
        at: usize,
        /// The condition name.
        cond: String,
    },
    /// Changes an `Event`'s name.
    SetEvent {
        /// The `Event` step.
        at: usize,
        /// The name.
        name: String,
    },
    /// Points a jump at another label (the panel's drag).
    Retarget {
        /// The jumping step.
        at: usize,
        /// Which jump.
        #[serde(default)]
        branch: Branch,
        /// The option index for `branch: option`.
        #[serde(default)]
        option: Option<usize>,
        /// The new target label.
        label: String,
    },
    /// Renames a label and every jump to it.
    RenameLabel {
        /// The old name.
        from: String,
        /// The new name.
        to: String,
    },
}

/// Why an op was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpError {
    /// The op's index in the batch.
    pub index: usize,
    /// The lowercase reason code.
    pub reason: &'static str,
    /// What is wrong.
    pub message: String,
}

fn fail(reason: &'static str, message: impl Into<String>) -> (&'static str, String) {
    (reason, message.into())
}

fn materialize(asset: &mut ConversationAsset) {
    if asset.steps.is_empty() {
        asset.steps = asset.effective_steps();
    }
}

fn step_mut(asset: &mut ConversationAsset, at: usize) -> Result<&mut Step, (&'static str, String)> {
    materialize(asset);
    let len = asset.steps.len();
    asset
        .steps
        .get_mut(at)
        .ok_or_else(|| fail("bad_step", format!("no step {at} (the conversation has {len})")))
}

fn non_empty(what: &str, value: &str) -> Result<(), (&'static str, String)> {
    if value.trim().is_empty() {
        Err(fail("empty_name", format!("{what} must not be empty")))
    } else {
        Ok(())
    }
}

fn apply_one(asset: &mut ConversationAsset, op: &ConvOp) -> Result<(), (&'static str, String)> {
    match op {
        ConvOp::AddLine {
            id,
            speaker,
            text,
            at,
        } => {
            non_empty("the line id", id)?;
            if asset.line(id).is_some() {
                return Err(fail("duplicate_line", format!("line `{id}` already exists")));
            }
            materialize(asset);
            let at = at.unwrap_or(asset.steps.len());
            if at > asset.steps.len() {
                return Err(fail("bad_step", format!("cannot insert at {at}")));
            }
            asset.lines.push(ConversationLine {
                id: id.clone(),
                speaker: speaker.clone(),
                text: text.clone(),
                note: String::new(),
                emotion: None,
                tags: Vec::new(),
                voice: Default::default(),
                duration: Default::default(),
                cues: Vec::new(),
                priority: None,
                keep_breaks: false,
            });
            asset.steps.insert(at, Step::Line(id.clone()));
        }
        ConvOp::AddStep { step, at } => {
            if let NewStep::Line { line } = step
                && asset.line(line).is_none()
            {
                return Err(fail(
                    "unknown_line",
                    format!("no line `{line}`: use add_line to create one"),
                ));
            }
            materialize(asset);
            let at = at.unwrap_or(asset.steps.len());
            if at > asset.steps.len() {
                return Err(fail("bad_step", format!("cannot insert at {at}")));
            }
            asset.steps.insert(at, step.clone().into_step());
        }
        ConvOp::DeleteStep { at } => {
            step_mut(asset, *at)?;
            let removed = asset.steps.remove(*at);
            if let Step::Line(id) = removed
                && !asset
                    .steps
                    .iter()
                    .any(|s| matches!(s, Step::Line(other) if *other == id))
            {
                asset.lines.retain(|l| l.id != id);
                if asset.steps.is_empty() && !asset.lines.is_empty() {
                    // An empty list would mean "every line in file order"; keep the meaning.
                    asset.steps.push(Step::End);
                }
            }
        }
        ConvOp::MoveStep { from, to } => {
            step_mut(asset, *from)?;
            if *to >= asset.steps.len() {
                return Err(fail("bad_step", format!("no step {to}")));
            }
            let step = asset.steps.remove(*from);
            asset.steps.insert(*to, step);
        }
        ConvOp::SetLine {
            line,
            text,
            speaker,
            note,
            emotion,
        } => {
            let Some(l) = asset.lines.iter_mut().find(|l| l.id == *line) else {
                return Err(fail("unknown_line", format!("no line `{line}`")));
            };
            if let Some(text) = text {
                non_empty("the text key", text)?;
                l.text.clone_from(text);
            }
            if let Some(speaker) = speaker {
                non_empty("the speaker", speaker)?;
                l.speaker.clone_from(speaker);
            }
            if let Some(note) = note {
                l.note.clone_from(note);
            }
            if let Some(emotion) = emotion {
                l.emotion = (!emotion.is_empty()).then(|| emotion.clone());
            }
        }
        ConvOp::SetPrompt { at, prompt } => match step_mut(asset, *at)? {
            Step::Choice { prompt: p, .. } => *p = (!prompt.is_empty()).then(|| prompt.clone()),
            _ => return Err(fail("wrong_step", format!("step {at} is not a Choice"))),
        },
        ConvOp::AddOption { at, text, cond, goto } => match step_mut(asset, *at)? {
            Step::Choice { options, .. } => options.push(
                NewOption {
                    text: text.clone(),
                    cond: cond.clone(),
                    goto: goto.clone(),
                }
                .into_option(),
            ),
            _ => return Err(fail("wrong_step", format!("step {at} is not a Choice"))),
        },
        ConvOp::SetOption {
            at,
            option,
            text,
            cond,
            goto,
        } => {
            let Step::Choice { options, .. } = step_mut(asset, *at)? else {
                return Err(fail("wrong_step", format!("step {at} is not a Choice")));
            };
            let count = options.len();
            let Some(o) = options.get_mut(*option) else {
                return Err(fail(
                    "bad_option",
                    format!("no option {option} (the choice has {count})"),
                ));
            };
            if let Some(text) = text {
                o.text.clone_from(text);
            }
            if let Some(cond) = cond {
                o.cond = (!cond.is_empty()).then(|| cond.clone());
            }
            if let Some(goto) = goto {
                o.goto.clone_from(goto);
            }
        }
        ConvOp::DeleteOption { at, option } => {
            let Step::Choice { options, .. } = step_mut(asset, *at)? else {
                return Err(fail("wrong_step", format!("step {at} is not a Choice")));
            };
            if *option >= options.len() {
                return Err(fail("bad_option", format!("no option {option}")));
            }
            options.remove(*option);
        }
        ConvOp::MoveOption { at, from, to } => {
            let Step::Choice { options, .. } = step_mut(asset, *at)? else {
                return Err(fail("wrong_step", format!("step {at} is not a Choice")));
            };
            if *from >= options.len() || *to >= options.len() {
                return Err(fail("bad_option", format!("no option {from} or {to}")));
            }
            let o = options.remove(*from);
            options.insert(*to, o);
        }
        ConvOp::SetCond { at, cond } => {
            non_empty("the condition", cond)?;
            match step_mut(asset, *at)? {
                Step::If { cond: c, .. } => c.clone_from(cond),
                _ => return Err(fail("wrong_step", format!("step {at} is not an If"))),
            }
        }
        ConvOp::SetEvent { at, name } => {
            non_empty("the event name", name)?;
            match step_mut(asset, *at)? {
                Step::Event(n) => n.clone_from(name),
                _ => return Err(fail("wrong_step", format!("step {at} is not an Event"))),
            }
        }
        ConvOp::Retarget {
            at,
            branch,
            option,
            label,
        } => {
            non_empty("the label", label)?;
            let step = step_mut(asset, *at)?;
            let slot = match (step, branch) {
                (Step::Goto(l), Branch::Goto) => l,
                (Step::If { then, .. }, Branch::Then) => then,
                (Step::If { else_, .. }, Branch::Else) => else_,
                (Step::Choice { options, .. }, Branch::Option) => {
                    let k = option.unwrap_or(0);
                    let count = options.len();
                    match options.get_mut(k) {
                        Some(o) => &mut o.goto,
                        None => {
                            return Err(fail(
                                "bad_option",
                                format!("no option {k} (the choice has {count})"),
                            ));
                        }
                    }
                }
                _ => {
                    return Err(fail("wrong_step", format!("step {at} has no {branch:?} jump")));
                }
            };
            slot.clone_from(label);
        }
        ConvOp::RenameLabel { from, to } => {
            non_empty("the label", to)?;
            materialize(asset);
            if !asset
                .steps
                .iter()
                .any(|s| matches!(s, Step::Label(l) if l == from))
            {
                return Err(fail("unknown_label", format!("no label `{from}`")));
            }
            if from != to && asset.steps.iter().any(|s| matches!(s, Step::Label(l) if l == to)) {
                return Err(fail("duplicate_label", format!("label `{to}` already exists")));
            }
            let swap = |l: &mut String| {
                if l == from {
                    l.clone_from(to);
                }
            };
            for step in &mut asset.steps {
                match step {
                    Step::Label(l) | Step::Goto(l) => swap(l),
                    Step::If { then, else_, .. } => {
                        swap(then);
                        swap(else_);
                    }
                    Step::Choice { options, .. } => options.iter_mut().for_each(|o| swap(&mut o.goto)),
                    Step::Line(_) | Step::Event(_) | Step::End => {}
                }
            }
        }
    }
    Ok(())
}

/// Applies `ops` in order to a copy of `asset`. On the first refused op nothing is applied.
///
/// # Errors
///
/// The refused op's index, reason code and message.
pub fn apply_ops(asset: &ConversationAsset, ops: &[ConvOp]) -> Result<ConversationAsset, OpError> {
    let mut out = asset.clone();
    for (index, op) in ops.iter().enumerate() {
        apply_one(&mut out, op).map_err(|(reason, message)| OpError {
            index,
            reason,
            message,
        })?;
    }
    Ok(out)
}
