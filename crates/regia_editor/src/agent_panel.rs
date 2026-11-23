// SPDX-License-Identifier: MPL-2.0
//! Keyboard-operable hosted Agent panel (`book/agents/agent-panel.md`).

use crate::agent_parity as actions;
use crate::agent_sessions::{Mode, agent_sessions};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

/// View state only; session state lives in `agent_sessions` and keeps running with the tab closed.
#[derive(Resource, Default, Clone)]
pub struct AgentPanel {
    command: String,
    selected: Option<u64>,
    text: String,
    status: String,
    checked: Vec<bool>,
    plan: Value,
    /// Ids of the staged calls checked for Approve or Reject checked.
    staged_checked: Vec<u64>,
    last_action_frame: Option<u64>,
}

/// Draws the Agent panel. All state-changing controls use the same operation as the protocol.
pub fn panel(ui: &mut egui::Ui, world: &mut World) {
    let _span = tracing::trace_span!("agent_panel").entered();
    crate::agent_sessions::set_wake(world, ui.ctx());
    let mut panel = world.get_resource::<AgentPanel>().cloned().unwrap_or_default();
    let state = match agent_sessions(world, &json!({"action":"list"})) {
        Ok(state) => state,
        Err(error) => {
            ui.label(error.message);
            return;
        }
    };
    let commands = state["commands"].as_array().cloned().unwrap_or_default();
    let sessions = state["sessions"].as_array().cloned().unwrap_or_default();
    let mut action = None;
    let mut open_preferences = false;
    if commands.is_empty() {
        ui.label("Configure Agent commands in Preferences to enable this panel.");
        if actions::action_button(ui, &actions::AGENT_PREFERENCES).clicked() {
            open_preferences = true;
        }
    } else {
        if !commands.iter().any(|name| name.as_str() == Some(&panel.command)) {
            panel.command = commands[0].as_str().unwrap_or_default().to_string();
        }
        ui.horizontal(|ui| {
            egui::ComboBox::from_label("Agent command")
                .selected_text(&panel.command)
                .show_ui(ui, |ui| {
                    for command in &commands {
                        if let Some(command) = command.as_str()
                            && actions::action_button_text(ui, &actions::AGENT_PICK, command).clicked()
                        {
                            panel.command = command.to_string();
                            ui.close();
                        }
                    }
                });
            if actions::action_button(ui, &actions::AGENT_START).clicked() {
                action = Some(json!({"action":"start", "command":panel.command, "mode":"ask"}));
            }
        });
    }
    if panel
        .selected
        .is_none_or(|id| !sessions.iter().any(|s| s["id"].as_u64() == Some(id)))
    {
        panel.selected = sessions.last().and_then(|s| s["id"].as_u64());
    }
    if let Some(session) = sessions.iter().find(|s| s["id"].as_u64() == panel.selected) {
        let id = session["id"].as_u64().unwrap_or_default();
        ui.horizontal(|ui| {
            egui::ComboBox::from_label("Session")
                .selected_text(format!("{} · {id}", session["name"].as_str().unwrap_or_default()))
                .show_ui(ui, |ui| {
                    for session in &sessions {
                        if actions::action_button_text(
                            ui,
                            &actions::AGENT_PICK,
                            format!(
                                "{} · {}",
                                session["name"].as_str().unwrap_or_default(),
                                session["id"]
                            ),
                        )
                        .clicked()
                        {
                            panel.selected = session["id"].as_u64();
                            ui.close();
                        }
                    }
                });
            if actions::action_enabled(ui, session["running"] == true, &actions::AGENT_STOP, None).clicked() {
                action = Some(json!({"action":"stop", "session":id}));
            }
        });
        let mode: Mode = serde_json::from_value(session["mode"].clone()).unwrap_or_default();
        ui.label(format!("Active mode: {}", mode.label()));
        if mode == Mode::Propose {
            ui.label(
                "File proposals appear in Agent Changes. Live editor calls are staged below for approval.",
            );
        }
        ui.horizontal(|ui| {
            for next in [Mode::Ask, Mode::Propose, Mode::Act] {
                if actions::action_enabled(
                    ui,
                    session["running"] == true && mode != next,
                    &actions::AGENT_MODE,
                    Some(next.label().into()),
                )
                .clicked()
                {
                    action = Some(json!({"action":"mode", "session":id, "mode":next}));
                }
            }
        });
        if session["history_error"] == true {
            ui.label("History could not be saved completely.");
        }
        egui::ScrollArea::vertical()
            .id_salt("agent-transcript")
            .max_height(ui.available_height() * 0.5)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if let Some(messages) = session["messages"].as_array() {
                    for message in messages {
                        let mode: Mode = serde_json::from_value(message["mode"].clone()).unwrap_or_default();
                        ui.label(format!(
                            "[{}] {}: {}",
                            mode.label(),
                            message["role"].as_str().unwrap_or_default(),
                            message["text"].as_str().unwrap_or_default()
                        ));
                    }
                }
            });
        let plan = &session["plan"];
        if !plan.is_null() && plan["approved"] != true {
            let steps = plan["steps"].as_array().cloned().unwrap_or_default();
            let plan_key = json!([id, plan]);
            if panel.plan != plan_key {
                panel.plan = plan_key;
                panel.checked = vec![false; steps.len()];
            }
            ui.strong("Plan awaiting approval");
            egui::ScrollArea::vertical()
                .id_salt("agent-plan-steps")
                .max_height(ui.available_height() * 0.5)
                .show(ui, |ui| {
                    for (step, checked) in steps.iter().zip(&mut panel.checked) {
                        ui.horizontal(|ui| {
                            let label = ui.label(step.as_str().unwrap_or_default());
                            actions::action_toggle(ui, &actions::AGENT_PLAN_CHECK, checked)
                                .labelled_by(label.id);
                        });
                    }
                });
            ui.horizontal(|ui| {
                if actions::action_enabled(
                    ui,
                    session["running"] == true && panel.checked.iter().all(|c| *c),
                    &actions::AGENT_APPROVE,
                    None,
                )
                .clicked()
                {
                    action = Some(json!({"action":"send", "session":id, "approve_plan":true}));
                }
                if actions::action_enabled(ui, session["running"] == true, &actions::AGENT_REJECT, None)
                    .clicked()
                {
                    action = Some(json!({"action":"send", "session":id, "approve_plan":false}));
                }
            });
        }
        let staged = session["staged"].as_array().cloned().unwrap_or_default();
        panel
            .staged_checked
            .retain(|checked| staged.iter().any(|s| s["id"].as_u64() == Some(*checked)));
        if !staged.is_empty() {
            ui.strong(format!("Staged editor calls ({})", staged.len()));
            egui::ScrollArea::vertical()
                .id_salt("agent-staged-calls")
                .max_height(ui.available_height() * 0.4)
                .show(ui, |ui| {
                    for call in &staged {
                        let Some(call_id) = call["id"].as_u64() else {
                            continue;
                        };
                        let touches = call["preview"]["touches"]
                            .as_array()
                            .map(|t| t.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
                            .unwrap_or_default();
                        ui.horizontal(|ui| {
                            let mut on = panel.staged_checked.contains(&call_id);
                            let params = call["params"].as_str().unwrap_or_default();
                            let params: String = if params.chars().count() > 160 {
                                params.chars().take(160).chain(std::iter::once('…')).collect()
                            } else {
                                params.to_string()
                            };
                            let label = ui.label(format!(
                                "{call_id}. {} {params}: {}. Touches {touches}",
                                call["method"].as_str().unwrap_or_default(),
                                call["preview"]["summary"].as_str().unwrap_or_default(),
                            ));
                            if actions::action_toggle(ui, &actions::AGENT_STAGED_CHECK, &mut on)
                                .labelled_by(label.id)
                                .changed()
                            {
                                if on {
                                    panel.staged_checked.push(call_id);
                                } else {
                                    panel.staged_checked.retain(|c| *c != call_id);
                                }
                            }
                        });
                    }
                });
            let any = !panel.staged_checked.is_empty();
            let decide = |approve: bool, items: Option<&Vec<u64>>| match items {
                Some(items) => json!({"action":"staged", "session":id, "approve":approve, "items":items}),
                None => json!({"action":"staged", "session":id, "approve":approve}),
            };
            ui.horizontal(|ui| {
                if actions::action_button(ui, &actions::AGENT_STAGED_APPROVE_ALL).clicked() {
                    action = Some(decide(true, None));
                }
                if actions::action_enabled(ui, any, &actions::AGENT_STAGED_APPROVE, None).clicked() {
                    action = Some(decide(true, Some(&panel.staged_checked)));
                }
                if actions::action_enabled(ui, any, &actions::AGENT_STAGED_REJECT, None).clicked() {
                    action = Some(decide(false, Some(&panel.staged_checked)));
                }
                if actions::action_button(ui, &actions::AGENT_STAGED_REJECT_ALL).clicked() {
                    action = Some(decide(false, None));
                }
            });
        }
        let label = ui.label("Message to agent");
        let message = ui
            .add(egui::TextEdit::singleline(&mut panel.text).id(egui::Id::new("agent-message")))
            .labelled_by(label.id);
        let enter = message.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let send = actions::action_enabled(
            ui,
            session["running"] == true && !panel.text.trim().is_empty(),
            &actions::AGENT_SEND,
            None,
        )
        .clicked();
        if (enter || send) && session["running"] == true && !panel.text.trim().is_empty() {
            action = Some(json!({"action":"send", "session":id, "text":panel.text}));
        }
    }
    ui.label(&panel.status);
    // egui may rerun the panel during one frame. A repeated pass cannot launch or send twice.
    let frame = ui.ctx().cumulative_frame_nr();
    if open_preferences && panel.last_action_frame != Some(frame) {
        panel.last_action_frame = Some(frame);
        let _ = crate::command_palette::run(world, "window.preferences");
    }
    if let Some(action) = action
        && panel.last_action_frame != Some(frame)
    {
        panel.last_action_frame = Some(frame);
        match agent_sessions(world, &action) {
            Ok(reply) => {
                panel.selected = reply["id"].as_u64().or(panel.selected);
                if action["action"] == "send" {
                    panel.text.clear();
                }
                panel.status.clear();
            }
            Err(error) => panel.status = error.message,
        }
    }
    world.insert_resource(panel);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_panel_keyboard_enter_sends_once_at_scaled_ui() {
        let mut h = crate::agent_sessions::tests::Harness::new();
        let id = h.start(Mode::Ask);
        h.call(
            json!({"action":"send", "session":id, "text":json!({"fixture":"plan", "steps":32}).to_string()}),
        );
        h.wait_for(id, "Select the test entity");
        h.app.world_mut().insert_resource(AgentPanel {
            selected: Some(id),
            text: json!({"fixture":"cwd"}).to_string(),
            ..Default::default()
        });
        let ctx = egui::Context::default();
        ctx.set_zoom_factor(1.5);
        let draw = |ctx: &egui::Context, world: &mut World, events| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 800.0),
                )),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| panel(ui, world));
            });
            output.textures_delta.clear();
        };
        draw(&ctx, h.app.world_mut(), Vec::new());
        for _ in 0..64 {
            draw(
                &ctx,
                h.app.world_mut(),
                vec![egui::Event::Key {
                    key: egui::Key::Tab,
                    physical_key: Some(egui::Key::Tab),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
            draw(&ctx, h.app.world_mut(), Vec::new());
            if ctx.memory(|m| m.has_focus(egui::Id::new("agent-message"))) {
                break;
            }
        }
        assert!(
            ctx.memory(|m| m.has_focus(egui::Id::new("agent-message"))),
            "Tab reaches the message field without a pointer or programmatic focus"
        );
        draw(
            &ctx,
            h.app.world_mut(),
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: Some(egui::Key::Enter),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        h.wait_for(id, "cwd:");
        let state = h.call(json!({"action":"list"}));
        let messages = state["sessions"][0]["messages"].as_array().expect("transcript");
        assert_eq!(
            messages.iter().filter(|m| m["role"] == "user").count(),
            2,
            "one plan request plus one Enter message"
        );
        assert!(h.app.world().resource::<AgentPanel>().text.is_empty());
    }

    /// P-369: the staged-call list is keyboard-operable. Tab reaches a call's check, Space
    /// checks it, Tab reaches Approve checked calls and Enter runs it exactly once, even in a
    /// frame egui runs twice; the other staged call stays staged.
    #[test]
    fn agent_panel_staged_calls_approve_from_the_keyboard_once() {
        let mut h = crate::agent_sessions::tests::Harness::new();
        let id = h.start(Mode::Propose);
        for name in ["Keyboard staged", "Left staged"] {
            let answer = h.child_call(id, "editor.spawn", json!({"name":name}));
            assert!(answer.contains("\"staged\":true"), "{answer}");
        }
        h.app.world_mut().insert_resource(AgentPanel {
            selected: Some(id),
            ..Default::default()
        });
        let ctx = egui::Context::default();
        ctx.set_zoom_factor(1.5);
        let draw = |ctx: &egui::Context, world: &mut World, events, second_pass: bool| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 1000.0),
                )),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                if second_pass && ui.ctx().current_pass_index() == 0 {
                    ui.ctx().request_discard("test: rerun the panel in this frame");
                }
                egui::CentralPanel::default().show(ui, |ui| panel(ui, world));
            });
            output.textures_delta.clear();
            let focused: Vec<String> = output
                .platform_output
                .events
                .iter()
                .filter_map(|event| match event {
                    egui::output::OutputEvent::FocusGained(info) => info.label.clone(),
                    _ => None,
                })
                .collect();
            (output.platform_output.num_completed_passes, focused)
        };
        let key = |key| egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let tab_to = |world: &mut World, label: &str| {
            for _ in 0..96 {
                let (_, focused) = draw(&ctx, world, vec![key(egui::Key::Tab)], false);
                if focused.iter().any(|f| f == label) {
                    return true;
                }
            }
            false
        };
        draw(&ctx, h.app.world_mut(), Vec::new(), false);
        assert!(
            tab_to(h.app.world_mut(), actions::AGENT_STAGED_CHECK.label),
            "Tab reaches the first staged call's check"
        );
        draw(&ctx, h.app.world_mut(), vec![key(egui::Key::Space)], false);
        let checked = h.app.world().resource::<AgentPanel>().staged_checked.clone();
        assert_eq!(checked.len(), 1, "Space checked one staged call");
        assert!(
            tab_to(h.app.world_mut(), actions::AGENT_STAGED_APPROVE.label),
            "Tab reaches Approve checked calls"
        );
        let depth = h
            .app
            .world()
            .get_resource::<crate::command::UndoStack>()
            .map_or(0, crate::command::UndoStack::undo_depth);
        let (passes, _) = draw(&ctx, h.app.world_mut(), vec![key(egui::Key::Enter)], true);
        assert_eq!(passes, 2, "the frame reran the panel");
        let world = h.app.world_mut();
        assert_eq!(crate::agent_sessions::tests::named(world, "Keyboard staged"), 1);
        assert_eq!(crate::agent_sessions::tests::named(world, "Left staged"), 0);
        assert_eq!(
            world
                .get_resource::<crate::command::UndoStack>()
                .map_or(0, crate::command::UndoStack::undo_depth),
            depth + 1,
            "Enter approved once"
        );
        let state = h.call(json!({"action":"list"}));
        assert_eq!(
            state["sessions"][0]["staged"].as_array().map(Vec::len),
            Some(1),
            "{state}"
        );
    }

    /// Review g15 item 9 (disputed): egui gives a discarded frame's second pass no input events
    /// (`RawInput::take`), so a plan check toggled in the first pass is not flipped back.
    /// Both the keyboard and the pointer toggle land exactly once in a two-pass frame.
    #[test]
    fn agent_panel_plan_check_toggles_once_in_a_two_pass_frame() {
        let mut h = crate::agent_sessions::tests::Harness::new();
        let id = h.start(Mode::Ask);
        h.call(json!({"action":"send", "session":id, "text":json!({"fixture":"plan"}).to_string()}));
        h.wait_for(id, "Select the test entity");
        h.app.world_mut().insert_resource(AgentPanel {
            selected: Some(id),
            ..Default::default()
        });
        let ctx = egui::Context::default();
        // Returns the passes run this frame and the labels that gained keyboard focus.
        let draw = |ctx: &egui::Context, world: &mut World, events, second_pass: bool| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 800.0),
                )),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                if second_pass && ui.ctx().current_pass_index() == 0 {
                    ui.ctx().request_discard("test: rerun the panel in this frame");
                }
                egui::CentralPanel::default().show(ui, |ui| panel(ui, world));
            });
            output.textures_delta.clear();
            let focused: Vec<String> = output
                .platform_output
                .events
                .iter()
                .filter_map(|event| match event {
                    egui::output::OutputEvent::FocusGained(info) => info.label.clone(),
                    _ => None,
                })
                .collect();
            (output.platform_output.num_completed_passes, focused)
        };
        let key = |key| egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let checked = |world: &World| world.resource::<AgentPanel>().checked.clone();
        draw(&ctx, h.app.world_mut(), Vec::new(), false);
        let mut reached = false;
        for _ in 0..64 {
            let (_, focused) = draw(&ctx, h.app.world_mut(), vec![key(egui::Key::Tab)], false);
            if focused
                .iter()
                .any(|label| label == actions::AGENT_PLAN_CHECK.label)
            {
                reached = true;
                break;
            }
        }
        assert!(reached, "Tab reaches the first plan check");
        assert_eq!(checked(h.app.world()), vec![false, false]);
        let (passes, _) = draw(&ctx, h.app.world_mut(), vec![key(egui::Key::Space)], true);
        assert_eq!(passes, 2, "the frame reran the panel");
        assert_eq!(
            checked(h.app.world()),
            vec![true, false],
            "Space toggled the focused check exactly once"
        );
        let toggle = ctx.memory(|m| m.focused()).expect("focused plan check");
        let center = ctx
            .read_response(toggle)
            .expect("plan check response")
            .rect
            .center();
        draw(
            &ctx,
            h.app.world_mut(),
            vec![egui::Event::PointerMoved(center)],
            false,
        );
        let button = |pressed| egui::Event::PointerButton {
            pos: center,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let (passes, _) = draw(&ctx, h.app.world_mut(), vec![button(true), button(false)], true);
        assert_eq!(passes, 2, "the frame reran the panel");
        assert_eq!(
            checked(h.app.world()),
            vec![false, false],
            "a pointer click toggled the check exactly once"
        );
    }
}
