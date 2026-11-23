// SPDX-License-Identifier: MPL-2.0
//! Keyboard-operable Collaboration window; effects run once per cumulative egui frame.

use super::*;
use crate::agent_parity;
use serde_json::json;

#[derive(Resource)]
pub(super) struct PanelState {
    pub(super) open: bool,
    pub(super) address: String,
    pub(super) name: String,
    error: Option<String>,
    last_action_frame: Option<u64>,
}

impl Default for PanelState {
    fn default() -> Self {
        Self {
            open: false,
            address: "127.0.0.1:48100".into(),
            name: "Editor".into(),
            error: None,
            last_action_frame: None,
        }
    }
}

/// Toggles the Collaboration window (the command palette's keyboard entry).
pub fn toggle_window(world: &mut World) {
    let mut panel = world.get_resource_or_insert_with(PanelState::default);
    panel.open = !panel.open;
}

/// Draws the Collaboration window when opened from the command palette.
/// Window dimensions are egui points and inherit the editor's appearance scale.
pub fn show_window(ctx: &egui::Context, world: &mut World) {
    if !world.get_resource::<PanelState>().is_some_and(|p| p.open) {
        return;
    }
    let mut open = true;
    egui::Window::new("Collaboration")
        .open(&mut open)
        .default_width(380.0)
        .vscroll(true)
        .show(ctx, |ui| {
            panel(ui, world);
        });
    if !open {
        world.get_resource_or_insert_with(PanelState::default).open = false;
    }
    if world
        .get_resource::<Session>()
        .is_some_and(|s| s.transport.is_some())
    {
        ctx.request_repaint_after(INTERVAL);
    }
}

pub(super) fn panel(ui: &mut egui::Ui, world: &mut World) -> Vec<(String, egui::Id)> {
    let _span = tracing::info_span!(target: "regia_editor", "collab_panel").entered();
    let mut state = world.remove_resource::<PanelState>().unwrap_or_default();
    let frame = ui.ctx().cumulative_frame_nr();
    let status = agent::status(world);
    let running = status["state"] != "off";
    let mut controls = vec![];
    let mut request: Option<(&str, serde_json::Value)> = None;
    ui.label("LAN presence only. Each join needs host acceptance.");
    ui.label("Use a trusted LAN; names are not verified and traffic is unencrypted.");
    if !running {
        ui.horizontal(|ui| {
            let label = ui.label("Name");
            let response = ui.text_edit_singleline(&mut state.name).labelled_by(label.id);
            controls.push(("Name".into(), response.id));
        });
        ui.horizontal(|ui| {
            let label = ui.label("Address");
            let response = ui.text_edit_singleline(&mut state.address).labelled_by(label.id);
            controls.push(("Address".into(), response.id));
        });
        ui.horizontal(|ui| {
            let response = agent_parity::action_button(ui, &agent_parity::COLLAB_HOST);
            controls.push(("Host".into(), response.id));
            if response.clicked() {
                request = Some((
                    "editor.collab_host",
                    json!({"address":state.address,"name":state.name}),
                ));
            }
            let response = agent_parity::action_button(ui, &agent_parity::COLLAB_JOIN);
            controls.push(("Join".into(), response.id));
            if response.clicked() {
                request = Some((
                    "editor.collab_join",
                    json!({"address":state.address,"name":state.name}),
                ));
            }
        });
    } else {
        ui.label(format!("State: {}", status["state"].as_str().unwrap_or("off")));
        if let Some(address) = status["address"].as_str() {
            ui.label(format!("Local address: {address}"));
        }
        let response = agent_parity::action_button(ui, &agent_parity::COLLAB_LEAVE);
        controls.push(("Leave".into(), response.id));
        if response.clicked() {
            request = Some(("editor.collab_leave", json!({})));
        }
        let participants = world
            .get_resource::<Session>()
            .map(Session::participants)
            .unwrap_or_default();
        let self_id = world.get_resource::<Session>().and_then(Session::self_id);
        ui.separator();
        ui.heading("Participants");
        for participant in participants {
            ui.push_id(participant.id, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(
                        indicators::participant_color(world, &participant),
                        format!("● {} ({})", participant.name, participant.id),
                    );
                    if Some(participant.id) != self_id {
                        let response = agent_parity::action_button_text(
                            ui,
                            &agent_parity::COLLAB_JUMP,
                            format!("Jump to {} ({})", participant.name, participant.id),
                        );
                        controls.push((format!("Jump to {}", participant.id), response.id));
                        if response.clicked() {
                            request = Some(("editor.collab_jump", json!({"participant":participant.id})));
                        }
                    } else {
                        ui.label("You");
                    }
                });
            });
        }
        let pending = world
            .get_resource::<Session>()
            .map(Session::pending)
            .unwrap_or_default();
        if !pending.is_empty() {
            ui.heading("Waiting for acceptance");
        }
        for participant in pending {
            let Some(id) = participant["id"].as_u64() else {
                continue;
            };
            ui.push_id(("pending", id), |ui| {
                ui.label(format!(
                    "{} ({id}) — {}",
                    participant["name"].as_str().unwrap_or(""),
                    participant["address"].as_str().unwrap_or("")
                ));
                ui.horizontal(|ui| {
                    let response = agent_parity::action_button_text(
                        ui,
                        &agent_parity::COLLAB_ACCEPT,
                        format!("Accept {id}"),
                    );
                    controls.push((format!("Accept {id}"), response.id));
                    if response.clicked() {
                        request = Some(("editor.collab_host", json!({"action":"accept","participant":id})));
                    }
                    let response = agent_parity::action_button_text(
                        ui,
                        &agent_parity::COLLAB_REJECT,
                        format!("Reject {id}"),
                    );
                    controls.push((format!("Reject {id}"), response.id));
                    if response.clicked() {
                        request = Some(("editor.collab_host", json!({"action":"reject","participant":id})));
                    }
                });
            });
        }
    }
    let transport_error = status["error"].as_str().map(|code| match code {
        "collab_rejected" => "The host declined your join request.",
        "collab_timeout" => "The host stopped responding. Check the connection before joining again.",
        "collab_host_left" => "The host ended this collaboration session.",
        _ => "Presence traffic failed. Check the connection and restart the session.",
    });
    if let Some(error) = state.error.as_deref().or(transport_error) {
        ui.colored_label(crate::theme::tokens(world).error, error);
    }
    if let Some((method, params)) = request
        && state.last_action_frame != Some(frame)
    {
        state.last_action_frame = Some(frame);
        let handler = match method {
            "editor.collab_host" => agent::host,
            "editor.collab_join" => agent::join,
            "editor.collab_leave" => agent::leave,
            _ => agent::jump,
        };
        state.error = handler(world, &params).err().map(|e| e.message);
    }
    world.insert_resource(state);
    controls
}
