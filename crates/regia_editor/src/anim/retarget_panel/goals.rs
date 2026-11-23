// SPDX-License-Identifier: MPL-2.0
//! The Retarget panel's IK goals list (ADR-8476, `book/agents/retargeting.md`): the setup's
//! `ik_goals` (`.retarget.ron` v2), each an `[upper, lower, end]` chain of mapped roles with a mode
//! (`ScaledWorld` or `Contacts`) and a weight; a toggle previews the bake with the goals solved;
//! each goal shows its residual and planted slide on the preview clip (`Retargeter::check`).
//! `editor.retarget_goal_set` edits the same list.

use regia_animation::GoalCheck;
use regia_animation::retarget::{IkGoalMode, IkGoalSpec};

use super::*;

/// A goal-list edit from the panel UI, applied once per frame.
#[derive(Clone, Debug, PartialEq)]
pub enum GoalEdit {
    /// Append these goals.
    Add(Vec<IkGoalSpec>),
    /// Remove the goal at this index.
    Remove(usize),
}

fn unknown_goal(index: usize, len: usize) -> PanelError {
    PanelError::new(
        "retarget_unknown_goal",
        format!("there is no IK goal {index} (the map has {len})"),
    )
}

impl RetargetPanel {
    /// Adds `goal` (`index` `None`) or replaces goal `index`; returns its index.
    ///
    /// # Errors
    ///
    /// `retarget_bad_goal` (an empty or repeated role, a weight outside `0..=1`),
    /// `retarget_unknown_goal` (no goal at `index`).
    pub fn set_goal(&mut self, index: Option<usize>, goal: IkGoalSpec) -> Result<usize, PanelError> {
        goal.validate()
            .map_err(|e| PanelError::new("retarget_bad_goal", e))?;
        let len = self.setup.ik_goals.len();
        let index = match index {
            None => {
                self.setup.ik_goals.push(goal);
                len
            }
            Some(i) => {
                let slot = self
                    .setup
                    .ik_goals
                    .get_mut(i)
                    .ok_or_else(|| unknown_goal(i, len))?;
                if *slot == goal {
                    return Ok(i);
                }
                *slot = goal;
                i
            }
        };
        self.dirty = true;
        self.touch();
        Ok(index)
    }

    /// Removes goal `index` and returns it.
    ///
    /// # Errors
    ///
    /// `retarget_unknown_goal`.
    pub fn remove_goal(&mut self, index: usize) -> Result<IkGoalSpec, PanelError> {
        let len = self.setup.ik_goals.len();
        if index >= len {
            return Err(unknown_goal(index, len));
        }
        let goal = self.setup.ik_goals.remove(index);
        self.dirty = true;
        self.touch();
        Ok(goal)
    }

    /// Per goal, its residual and planted slide on the preview clip (`Retargeter::check`), cached
    /// per edit and clip.
    ///
    /// # Errors
    ///
    /// As [`Self::retargeter`]; `retarget_unknown_clip` without a preview clip.
    pub fn goal_checks(&mut self) -> Result<Vec<GoalCheck>, PanelError> {
        let retargeter = self.retargeter()?;
        let clip = self.preview_clip()?;
        if let Some((revision, name, checks)) = &self.goal_report
            && *revision == self.revision
            && *name == clip.name
        {
            return Ok(checks.clone());
        }
        let checks = if self.setup.ik_goals.is_empty() {
            Vec::new()
        } else {
            retargeter
                .check(Some(&clip))
                .clip
                .map(|c| c.goals)
                .unwrap_or_default()
        };
        self.goal_report = Some((self.revision, clip.name.clone(), checks.clone()));
        Ok(checks)
    }

    /// `clip` baked with the current map and goals, cached per edit and clip.
    pub(super) fn baked_preview(
        &mut self,
        retargeter: &Retargeter,
        clip: &Arc<AnimationClip>,
    ) -> Arc<AnimationClip> {
        if let Some((revision, name, baked)) = &self.baked
            && *revision == self.revision
            && *name == clip.name
        {
            return Arc::clone(baked);
        }
        let baked = Arc::new(retargeter.retarget_clip(clip));
        self.baked = Some((self.revision, clip.name.clone(), Arc::clone(&baked)));
        baked
    }

    /// Roles mapped on both rigs, in row order: what a goal can name.
    #[must_use]
    pub fn goal_roles(&self) -> Vec<String> {
        self.rows()
            .into_iter()
            .filter(|r| r.source.is_some() && r.target.is_some())
            .map(|r| r.role)
            .collect()
    }
}

/// Applies a UI edit; returns the status line.
pub(super) fn apply_edit(panel: &mut RetargetPanel, edit: GoalEdit) -> String {
    match edit {
        GoalEdit::Add(goals) => {
            let count = goals.len();
            for goal in goals {
                if let Err(e) = panel.set_goal(None, goal) {
                    return e.message;
                }
            }
            format!("Added {count} IK goal(s)")
        }
        GoalEdit::Remove(index) => match panel.remove_goal(index) {
            Ok(goal) => format!("Removed the {} goal", goal.end_role()),
            Err(e) => e.message,
        },
    }
}

fn mode_name(mode: IkGoalMode) -> &'static str {
    match mode {
        IkGoalMode::ScaledWorld => "ScaledWorld",
        IkGoalMode::Contacts => "Contacts",
    }
}

fn parse_mode(value: &Value) -> Result<IkGoalMode, PanelError> {
    match value.as_str() {
        Some("ScaledWorld" | "scaled_world") => Ok(IkGoalMode::ScaledWorld),
        Some("Contacts" | "contacts") => Ok(IkGoalMode::Contacts),
        _ => Err(PanelError::new(
            "retarget_bad_goal",
            "`mode` must be ScaledWorld or Contacts",
        )),
    }
}

fn parse_roles(value: &Value) -> Result<[String; 3], PanelError> {
    let bad = || {
        PanelError::new(
            "retarget_bad_goal",
            "`roles` must be three role names: [upper, lower, end]",
        )
    };
    let items = value.as_array().filter(|a| a.len() == 3).ok_or_else(bad)?;
    let name = |i: usize| items[i].as_str().map(str::to_string).ok_or_else(bad);
    Ok([name(0)?, name(1)?, name(2)?])
}

/// `editor.retarget_goal_set { index?, roles?, mode?, weight?, preset?, remove?, preview? }`.
fn retarget_goal_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut panel = world.get_resource_or_insert_with(RetargetPanel::default);
    panel.rigs().map_err(rpc)?;
    let mut changed: Vec<&str> = Vec::new();
    let index = match params.get("index") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().map(|i| i as usize).ok_or_else(|| {
            rpc(PanelError::new(
                "retarget_unknown_goal",
                "`index` must be a goal index (0-based)",
            ))
        })?),
    };
    if params.get("remove").and_then(Value::as_bool) == Some(true) {
        let index = index.ok_or_else(|| {
            rpc(PanelError::new(
                "retarget_unknown_goal",
                "`remove` needs the goal's `index`",
            ))
        })?;
        panel.remove_goal(index).map_err(rpc)?;
        changed.push("remove");
    } else if let Some(preset) = params.get("preset") {
        let goals = match preset.as_str() {
            Some("feet") => IkGoalSpec::humanoid_feet(),
            Some("hands") => IkGoalSpec::humanoid_hands(),
            _ => {
                return Err(rpc(PanelError::new(
                    "retarget_bad_goal",
                    "`preset` must be feet or hands",
                )));
            }
        };
        for goal in goals {
            panel.set_goal(None, goal).map_err(rpc)?;
        }
        changed.push("preset");
    } else if ["roles", "mode", "weight"]
        .iter()
        .any(|k| params.get(*k).is_some())
    {
        let mut goal = match index {
            Some(i) => panel
                .setup
                .ik_goals
                .get(i)
                .cloned()
                .ok_or_else(|| rpc(unknown_goal(i, panel.setup.ik_goals.len())))?,
            None => {
                let roles = params.get("roles").ok_or_else(|| {
                    rpc(PanelError::new(
                        "retarget_bad_goal",
                        "a new goal needs `roles` [upper, lower, end]",
                    ))
                })?;
                IkGoalSpec {
                    roles: parse_roles(roles).map_err(rpc)?,
                    mode: IkGoalMode::ScaledWorld,
                    weight: 1.0,
                }
            }
        };
        if let Some(roles) = params.get("roles") {
            goal.roles = parse_roles(roles).map_err(rpc)?;
            changed.push("roles");
        }
        if let Some(mode) = params.get("mode") {
            goal.mode = parse_mode(mode).map_err(rpc)?;
            changed.push("mode");
        }
        if let Some(weight) = params.get("weight") {
            goal.weight = weight.as_f64().map(|w| w as f32).ok_or_else(|| {
                rpc(PanelError::new(
                    "retarget_bad_goal",
                    "`weight` must be a number in 0..=1",
                ))
            })?;
            changed.push("weight");
        }
        panel.set_goal(index, goal).map_err(rpc)?;
    }
    if let Some(preview) = params.get("preview").and_then(Value::as_bool) {
        panel.goal_preview = preview;
        changed.push("preview");
    }
    // The checks need a preview clip; a rig without clips reports none.
    let checks = match panel.goal_checks() {
        Ok(checks) => checks,
        Err(e) if e.code == "retarget_unknown_clip" => Vec::new(),
        Err(e) => return Err(rpc(e)),
    };
    let mut out = state_json(&panel);
    out["changed"] = json!(changed);
    out["checks"] = serde_json::to_value(&checks).map_err(|e| RpcError::internal(e.to_string()))?;
    Ok(out)
}

const GOAL_SET_PARAMS: &[ParamSchema] = &[
    param(
        "index",
        "integer",
        false,
        "The goal to change or remove (0-based); absent adds a goal.",
    ),
    param(
        "roles",
        "array",
        false,
        "[upper, lower, end] role names, each mapped on both rigs.",
    ),
    param(
        "mode",
        "string",
        false,
        "ScaledWorld (default for a new goal) or Contacts.",
    ),
    param("weight", "number", false, "The goal's blend, 0..=1 (default 1)."),
    param(
        "preset",
        "string",
        false,
        "feet (Contacts goals on both legs) or hands (ScaledWorld goals on both arms): appended.",
    ),
    param("remove", "boolean", false, "Remove the goal at `index`."),
    param(
        "preview",
        "boolean",
        false,
        "Preview the bake with the goals solved.",
    ),
];

/// The `editor.retarget_goal_set` schema and handler.
pub(super) fn method() -> (MethodSchema, Handler) {
    (
        MethodSchema {
            name: "editor.retarget_goal_set",
            tool: "retarget_goal_set",
            description: "Edit the map's IK goals (.retarget.ron v2 `ik_goals`, solved at bake): add one \
                (`roles` [upper, lower, end] of mapped roles, `mode` ScaledWorld or Contacts, `weight`), change \
                goal `index`, add a `preset` (feet: Contacts goals on both legs; hands: ScaledWorld goals on both \
                arms), or `remove` goal `index`; `preview` toggles the preview of the bake with goals. Returns \
                the panel state and, per goal, its residual and planted slide on the preview clip.",
            params: GOAL_SET_PARAMS,
            result: "the panel state plus { changed, checks: [{ roles, mode, weight, resolved, max_residual, \
                mean_residual, planted_frames, planted_slide }] }",
            mutating: true,
            permission: Permission::EditScene,
        },
        retarget_goal_set as Handler,
    )
}

fn role_combo(ui: &mut egui::Ui, id: (usize, usize), roles: &[String], current: &str) -> Option<String> {
    let mut picked = None;
    let response = egui::ComboBox::from_id_salt(("retarget_goal_role", id))
        .selected_text(current)
        .show_ui(ui, |ui| {
            for role in roles {
                if ui.pick_row(role == current, role) {
                    picked = Some(role.clone());
                }
            }
        })
        .response;
    let part = ["upper", "lower", "end"][id.1.min(2)];
    crate::a11y_audit::name(
        response,
        egui::WidgetType::ComboBox,
        &format!("IK goal {} {part} role", id.0 + 1),
    );
    picked.filter(|p| p != current)
}

/// Draws the goals list; adds and removes go through `edit` (applied once per frame).
pub(super) fn goal_list(
    ui: &mut egui::Ui,
    panel: &mut RetargetPanel,
    tokens: &crate::theme::ThemeTokens,
    edit: &mut Option<GoalEdit>,
) {
    ui.separator();
    ui.strong("IK goals");
    ui.horizontal_wrapped(|ui| {
        if ui.button("Add feet goals").clicked() {
            *edit = Some(GoalEdit::Add(IkGoalSpec::humanoid_feet()));
        }
        if ui.button("Add hand goals").clicked() {
            *edit = Some(GoalEdit::Add(IkGoalSpec::humanoid_hands()));
        }
        ui.checkbox(&mut panel.goal_preview, "Preview with goals");
    });
    if panel.setup.ik_goals.is_empty() {
        ui.colored_label(
            tokens.muted,
            "No goals: the bake keeps the target's bone lengths, so planted feet may slide.",
        );
        return;
    }
    let roles = panel.goal_roles();
    let checks = panel.goal_checks().unwrap_or_default();
    let goals = panel.setup.ik_goals.clone();
    egui::Grid::new("retarget_goals")
        .num_columns(6)
        .striped(true)
        .show(ui, |ui| {
            for label in ["Upper", "Lower", "End", "Mode", "Weight", ""] {
                ui.colored_label(tokens.muted, label);
            }
            ui.end_row();
            for (i, goal) in goals.iter().enumerate() {
                let mut next = goal.clone();
                for (k, role) in goal.roles.iter().enumerate() {
                    if let Some(pick) = role_combo(ui, (i, k), &roles, role) {
                        next.roles[k] = pick;
                    }
                }
                let response = egui::ComboBox::from_id_salt(("retarget_goal_mode", i))
                    .selected_text(mode_name(goal.mode))
                    .show_ui(ui, |ui| {
                        for mode in [IkGoalMode::ScaledWorld, IkGoalMode::Contacts] {
                            if ui.pick_row(goal.mode == mode, mode_name(mode)) {
                                next.mode = mode;
                            }
                        }
                    })
                    .response;
                crate::a11y_audit::name(
                    response,
                    egui::WidgetType::ComboBox,
                    &format!("IK goal {} mode", i + 1),
                );
                let r = ui.add(
                    egui::DragValue::new(&mut next.weight)
                        .speed(0.01)
                        .range(0.0..=1.0),
                );
                crate::a11y_audit::named_drag_value(
                    r,
                    &format!("IK goal {} weight", i + 1),
                    f64::from(next.weight),
                );
                if ui.small_button("Remove").clicked() {
                    *edit = Some(GoalEdit::Remove(i));
                }
                ui.end_row();
                if next != *goal
                    && let Err(e) = panel.set_goal(Some(i), next)
                {
                    panel.status = Some(e.message);
                }
                if let Some(check) = checks.get(i) {
                    let slide = check
                        .planted_slide
                        .map_or_else(String::new, |s| format!(", planted slide {:.1} cm", s * 100.0));
                    let text = if check.resolved {
                        format!("residual {:.1} cm{slide}", check.max_residual * 100.0)
                    } else {
                        "not mapped on both rigs (skipped)".to_string()
                    };
                    ui.colored_label(tokens.muted, text);
                    ui.end_row();
                }
            }
        });
}
