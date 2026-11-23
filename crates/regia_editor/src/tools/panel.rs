// SPDX-License-Identifier: MPL-2.0
//! The Tools panel (`Panel::Tool`, ADR-1523): the tool list with launch and stop on the left,
//! the selected tool on the right (tasks, its own view, log, provided methods). It draws the
//! [`ToolHost`] resource the `editor.tool_*` methods change, so a person and an agent see one
//! state.

use std::time::{Duration, Instant};

use regia_ecs::prelude::*;

use super::host::{TaskState, ToolHost, ToolRuntime, ToolStatus, ViewItem};
use super::manifest::DiscoveredTool;

/// How often the panel repaints while a tool runs a task or has a call in flight.
const BUSY_REPAINT: Duration = Duration::from_millis(250);

/// What a click in the panel asks for; applied after drawing (a launch needs the world).
enum Action {
    Select(String),
    Launch(String),
    Stop(String, bool),
    Click(String, String),
    Refresh,
}

/// Draws the Tools panel.
pub fn tools_panel(ui: &mut egui::Ui, world: &mut World) {
    let project = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone());
    let mut actions = Vec::new();
    {
        let mut host = world.get_resource_or_insert_with(ToolHost::default);
        host.ensure_discovered(project.as_deref());
        if host.selected.is_none() {
            host.selected = host.discovered().first().map(DiscoveredTool::id);
        }
        draw(ui, &host, &mut actions);
        if host.is_busy() {
            ui.ctx().request_repaint_after(BUSY_REPAINT);
        }
    }
    for action in actions {
        apply(world, action, project.as_deref());
    }
}

fn apply(world: &mut World, action: Action, project: Option<&std::path::Path>) {
    let result = match action {
        Action::Select(id) => {
            world.resource_mut::<ToolHost>().selected = Some(id);
            Ok(())
        }
        Action::Launch(id) => super::agent::launch_tool(world, &id),
        Action::Stop(id, force) => world.resource_mut::<ToolHost>().stop(&id, force, Instant::now()),
        Action::Click(id, item) => world.resource_mut::<ToolHost>().click(&id, &item),
        Action::Refresh => {
            world.resource_mut::<ToolHost>().refresh(project);
            Ok(())
        }
    };
    world.resource_mut::<ToolHost>().last_error = result.err().map(|e| format!("{} ({})", e.message, e.code));
}

fn status_color(ui: &egui::Ui, status: &ToolStatus) -> egui::Color32 {
    match status {
        ToolStatus::Running => egui::Color32::from_rgb(90, 190, 110),
        ToolStatus::Stopping => egui::Color32::from_rgb(220, 180, 70),
        ToolStatus::Failed(_) => egui::Color32::from_rgb(220, 90, 80),
        ToolStatus::Stopped | ToolStatus::Exited(_) => ui.visuals().weak_text_color(),
    }
}

fn draw(ui: &mut egui::Ui, host: &ToolHost, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.strong("Tools");
        ui.weak(format!("{} found", host.discovered().len()));
        if ui.small_button("Refresh").clicked() {
            actions.push(Action::Refresh);
        }
    });
    if let Some(error) = &host.last_error {
        ui.colored_label(egui::Color32::from_rgb(220, 90, 80), error);
    }
    ui.separator();
    if host.discovered().is_empty() {
        ui.weak("No tool manifests. Put an <id>.tool.toml in <project>/.regia/tools/, the user tools folder, or a REGIA_TOOL_PATH folder.");
        return;
    }
    ui.columns_const(|[left, right]: &mut [egui::Ui; 2]| {
        egui::ScrollArea::vertical()
            .id_salt("tools-list")
            .show(left, |ui| {
                for tool in host.discovered() {
                    let id = tool.id();
                    let status = host.status(&id);
                    let title = match &tool.manifest {
                        Ok(manifest) => manifest.title().to_string(),
                        Err(_) => format!("{id} (invalid)"),
                    };
                    ui.horizontal(|ui| {
                        ui.colored_label(status_color(ui, &status), "●");
                        let selected = host.selected.as_deref() == Some(id.as_str());
                        if ui.selectable_label(selected, title).clicked() {
                            actions.push(Action::Select(id.clone()));
                        }
                        ui.weak(status.name());
                    });
                }
            });
        let selected = host.selected.as_deref().and_then(|id| host.find(id));
        match selected {
            Some(tool) => draw_tool(right, host, tool, actions),
            None => {
                right.weak("Select a tool.");
            }
        }
    });
}

fn draw_tool(ui: &mut egui::Ui, host: &ToolHost, tool: &DiscoveredTool, actions: &mut Vec<Action>) {
    let id = tool.id();
    let status = host.status(&id);
    let manifest = match &tool.manifest {
        Ok(manifest) => manifest,
        Err(error) => {
            ui.strong(id.as_str());
            ui.colored_label(egui::Color32::from_rgb(220, 90, 80), error);
            ui.weak(tool.path.display().to_string());
            return;
        }
    };
    ui.horizontal(|ui| {
        ui.heading(manifest.title());
        if !manifest.version.is_empty() {
            ui.weak(&manifest.version);
        }
    });
    if !manifest.description.is_empty() {
        ui.label(&manifest.description);
    }
    ui.weak(format!("{} · {}", tool.source.name(), tool.path.display()));
    ui.horizontal(|ui| {
        let live = status.is_live();
        if ui.add_enabled(!live, egui::Button::new("Launch")).clicked() {
            actions.push(Action::Launch(id.clone()));
        }
        if ui
            .add_enabled(status == ToolStatus::Running, egui::Button::new("Stop"))
            .clicked()
        {
            actions.push(Action::Stop(id.clone(), false));
        }
        if status == ToolStatus::Stopping && ui.button("Kill").clicked() {
            actions.push(Action::Stop(id.clone(), true));
        }
        ui.colored_label(status_color(ui, &status), status_text(&status));
        if let Some(url) = &manifest.panel.url {
            ui.hyperlink_to("Open in browser", url);
        }
    });
    let Some(runtime) = host.runtime(&id) else {
        return;
    };
    ui.separator();
    draw_tasks(ui, runtime);
    draw_view(ui, runtime, &id, actions);
    egui::CollapsingHeader::new(format!(
        "Log ({} lines)",
        runtime.log_tail(super::host::LOG_LINES).len()
    ))
    .id_salt(("tool-log", &id))
    .show(ui, |ui| {
        egui::ScrollArea::vertical()
            .id_salt(("tool-log-scroll", &id))
            .max_height(200.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in runtime.log_tail(super::host::LOG_LINES) {
                    ui.monospace(line);
                }
            });
    });
    if !manifest.provides.is_empty() {
        egui::CollapsingHeader::new(format!("Methods ({})", manifest.provides.len()))
            .id_salt(("tool-methods", &id))
            .show(ui, |ui| {
                for method in &manifest.provides {
                    ui.horizontal(|ui| {
                        ui.monospace(&method.name);
                        ui.weak(&method.permission);
                        ui.label(&method.description);
                    });
                }
            });
    }
}

fn status_text(status: &ToolStatus) -> String {
    match status {
        ToolStatus::Exited(Some(code)) => format!("exited ({code})"),
        ToolStatus::Failed(message) => format!("failed: {message}"),
        other => other.name().to_string(),
    }
}

fn draw_tasks(ui: &mut egui::Ui, runtime: &ToolRuntime) {
    for task in runtime.tasks() {
        ui.horizontal(|ui| {
            let bar = match task.fraction {
                Some(fraction) => egui::ProgressBar::new(fraction).show_percentage(),
                None => egui::ProgressBar::new(0.0).animate(task.state == TaskState::Running),
            };
            ui.add(bar.text(task.label.clone()).desired_width(220.0));
            ui.weak(task.state.name());
            if !task.message.is_empty() {
                ui.label(&task.message);
            }
        });
    }
}

fn draw_view(ui: &mut egui::Ui, runtime: &ToolRuntime, id: &str, actions: &mut Vec<Action>) {
    if runtime.view().is_empty() {
        return;
    }
    ui.separator();
    let running = *runtime.status() == ToolStatus::Running;
    for item in runtime.view() {
        match item {
            ViewItem::Heading(text) => {
                ui.strong(text);
            }
            ViewItem::Label(text) => {
                ui.label(text);
            }
            ViewItem::Value { label, value } => {
                ui.horizontal(|ui| {
                    ui.weak(label);
                    ui.label(value);
                });
            }
            ViewItem::Progress { label, fraction } => {
                ui.add(
                    egui::ProgressBar::new(*fraction)
                        .text(label.clone())
                        .desired_width(220.0),
                );
            }
            ViewItem::Button { id: item, label } => {
                if ui.add_enabled(running, egui::Button::new(label)).clicked() {
                    actions.push(Action::Click(id.to_string(), item.clone()));
                }
            }
            ViewItem::Link { label, url } => {
                ui.hyperlink_to(label, url);
            }
            ViewItem::Separator => {
                ui.separator();
            }
        }
    }
}
