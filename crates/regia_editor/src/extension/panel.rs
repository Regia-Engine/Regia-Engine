// SPDX-License-Identifier: MPL-2.0
//! The Extensions panel (`specs/57` § 6): every discovered extension with state, version,
//! publisher, licence, requested versus granted capabilities, dependencies, diagnostics and the
//! enable and disable controls. It reads the JSON the `plugin.list` and `plugin.info` methods
//! answer and acts through the same functions, so a person and an agent see one state and no
//! action exists here that the protocol lacks.

use regia_ecs::prelude::*;
use regia_framework::extension::ExtensionRegistry;
use regia_framework::extension::manifest::IMPLICIT_CAPABILITIES;
use serde_json::{Value, json};

use super::context::{EditorExtensions, InspectorDraw};

/// The banner the panel always shows (spec § 2.7: never "sandboxed" or "safe").
pub const TRUST_BANNER: &str = "In-process extensions are trusted native code with the editor's full \
privileges. Capabilities are a review aid, not a sandbox. Install extensions only from a source you trust.";

/// The panel's own state: the selected extension and the last answer.
#[derive(Resource, Default)]
pub struct ExtensionsPanelState {
    /// The selected extension.
    pub selected: Option<String>,
    /// The last action's outcome, shown under the toolbar.
    pub message: String,
    /// The frame whose sample Open click was already applied.
    sample_frame: u64,
}

/// What a click asks for; applied after drawing (the handlers need the world).
#[derive(Debug, Clone, PartialEq)]
enum Action {
    Select(String),
    Enable(String, Vec<String>),
    Disable(String),
    Reload(String),
    /// Copy a bundled sample (its `editor.samples` id) into the Samples folder and open the copy.
    OpenSample(String),
}

/// The capabilities `plugin.enable` must be passed for an extension, from its `plugin.info`.
#[must_use]
pub fn grants_needed(info: &Value) -> Vec<String> {
    let granted: Vec<&str> = info["granted"]
        .as_array()
        .map(|rows| rows.iter().filter_map(|r| r["capability"].as_str()).collect())
        .unwrap_or_default();
    info["requested"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(Value::as_str)
                .filter(|c| !IMPLICIT_CAPABILITIES.contains(c) && !granted.contains(c))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Draws the Extensions panel.
pub fn extensions_panel(ui: &mut egui::Ui, world: &mut World) {
    // The theme's warning color (ADR-8589: the fixed gold was unreadable on the light theme).
    let warn = ui.visuals().warn_fg_color;
    ui.colored_label(warn, TRUST_BANNER);
    ui.separator();
    let Some(list) = world
        .get_resource::<ExtensionRegistry>()
        .map(|registry| registry.list_json(None))
    else {
        ui.weak("(open a project to see its extensions)");
        return;
    };
    let selected = world
        .get_resource_or_insert_with(ExtensionsPanelState::default)
        .selected
        .clone();
    let info = selected.as_deref().and_then(|id| {
        world
            .get_resource::<ExtensionRegistry>()
            .and_then(|registry| registry.info_json(id))
    });
    let mut actions = Vec::new();
    // The samples the selected extension bundles (`<extension>/samples/<name>/`, w13).
    let samples = match (&selected, &info) {
        (Some(id), Some(info)) => info["path"]
            .as_str()
            .and_then(|path| std::path::Path::new(path).parent())
            .map(|dir| crate::samples_browser::extension_samples(id, dir))
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let mut folder = world
        .get_resource_or_insert_with(crate::samples_browser::SamplesWindow::default)
        .folder
        .clone();
    let message = world
        .get_resource::<ExtensionsPanelState>()
        .map(|state| state.message.clone())
        .unwrap_or_default();
    ui.columns(2, |columns| {
        draw_list(&mut columns[0], &list, selected.as_deref(), &mut actions);
        let ui = &mut columns[1];
        if !message.is_empty() {
            ui.label(&message);
            ui.separator();
        }
        match &info {
            Some(info) => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    draw_info(ui, info, &mut actions);
                    if let Some(id) =
                        crate::samples_browser::extension_samples_section(ui, &mut folder, &samples)
                    {
                        actions.push(Action::OpenSample(id));
                    }
                });
            }
            None => {
                ui.weak("(select an extension)");
            }
        }
    });
    world
        .resource_mut::<crate::samples_browser::SamplesWindow>()
        .folder = folder;
    // egui can run a panel's UI twice in one frame: a click is applied once per frame.
    let frame = ui.ctx().cumulative_frame_nr();
    let fresh = world
        .get_resource_or_insert_with(ExtensionsPanelState::default)
        .sample_frame
        != frame;
    for action in actions {
        if matches!(action, Action::OpenSample(_)) {
            if !fresh {
                continue;
            }
            world.resource_mut::<ExtensionsPanelState>().sample_frame = frame;
        }
        apply(world, action);
    }
}

fn state_color(ui: &egui::Ui, state: &str) -> egui::Color32 {
    match state {
        "active" => crate::theme::ui_tokens(ui.ctx()).accents.ok,
        "failed" => crate::theme::ui_tokens(ui.ctx()).accents.error_text,
        _ => ui.visuals().weak_text_color(),
    }
}

fn draw_list(ui: &mut egui::Ui, list: &Value, selected: Option<&str>, actions: &mut Vec<Action>) {
    let rows = list["extensions"].as_array().map_or(&[][..], Vec::as_slice);
    if rows.is_empty() {
        ui.weak("(no extensions found)");
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        for row in rows {
            let id = row["id"].as_str().unwrap_or_default();
            let state = row["state"].as_str().unwrap_or_default();
            ui.horizontal(|ui| {
                ui.colored_label(state_color(ui, state), "●").on_hover_text(state);
                let text = format!("{id} {}", row["version"].as_str().unwrap_or_default());
                if ui.selectable_label(selected == Some(id), text).clicked() {
                    actions.push(Action::Select(id.to_owned()));
                }
            });
        }
    });
    let pending = list["restart_required"].as_array().map_or(0, Vec::len);
    if pending > 0 {
        ui.separator();
        ui.colored_label(
            ui.visuals().warn_fg_color,
            format!("Restart the editor: {pending} code change(s) pending"),
        );
    }
}

fn line(ui: &mut egui::Ui, label: &str, value: &Value) {
    if let Some(text) = value.as_str().filter(|text| !text.is_empty()) {
        ui.horizontal_wrapped(|ui| {
            ui.weak(label);
            ui.label(text);
        });
    }
}

fn draw_info(ui: &mut egui::Ui, info: &Value, actions: &mut Vec<Action>) {
    let id = info["id"].as_str().unwrap_or_default().to_owned();
    ui.strong(info["name"].as_str().filter(|n| !n.is_empty()).unwrap_or(&id));
    for (label, key) in [
        ("Version", "version"),
        ("State", "state"),
        ("Kind", "kind"),
        ("Publisher", "publisher"),
        ("Licence", "licence"),
        ("Description", "description"),
    ] {
        line(ui, label, &info[key]);
    }

    ui.separator();
    ui.strong("Capabilities");
    let needed = grants_needed(info);
    for capability in info["requested"].as_array().map_or(&[][..], Vec::as_slice) {
        let name = capability.as_str().unwrap_or_default();
        let grant = info["granted"]
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["capability"] == *capability))
            .and_then(|row| row["grant"].as_str())
            .unwrap_or("not granted");
        ui.label(format!("{name}: {grant}"));
    }

    let dependencies = info["dependencies"].as_array().map_or(&[][..], Vec::as_slice);
    if !dependencies.is_empty() {
        ui.separator();
        ui.strong("Dependencies");
        for dep in dependencies {
            ui.label(format!(
                "{} {} (found {})",
                dep["id"].as_str().unwrap_or_default(),
                dep["version"].as_str().unwrap_or_default(),
                dep["resolved"].as_str().unwrap_or("nothing"),
            ));
        }
    }

    if let Some(registrations) = info["registrations"].as_object().filter(|map| !map.is_empty()) {
        ui.separator();
        ui.strong("Registered");
        for (kind, names) in registrations {
            ui.label(format!("{kind}: {}", join(names)));
        }
    }

    let diagnostics = info["diagnostics"].as_array().map_or(&[][..], Vec::as_slice);
    if !diagnostics.is_empty() {
        ui.separator();
        ui.strong("Diagnostics");
        for diagnostic in diagnostics {
            ui.label(format!(
                "{} ({}): {}",
                diagnostic["code"].as_str().unwrap_or_default(),
                diagnostic["severity"].as_str().unwrap_or_default(),
                diagnostic["message"].as_str().unwrap_or_default(),
            ));
        }
    }

    ui.separator();
    ui.horizontal(|ui| {
        let state = info["state"].as_str().unwrap_or_default();
        if matches!(state, "available" | "disabled") {
            let label = if needed.is_empty() {
                "Enable".to_owned()
            } else {
                format!("Grant {} and enable", needed.join(", "))
            };
            if ui.button(label).clicked() {
                actions.push(Action::Enable(id.clone(), needed.clone()));
            }
        } else if ui.button("Disable").clicked() {
            actions.push(Action::Disable(id.clone()));
        }
        if !info["dynamic"].is_null() && ui.button("Reload").clicked() {
            actions.push(Action::Reload(id.clone()));
        }
    });
}

fn join(names: &Value) -> String {
    names
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn outcome(result: Result<Value, regia_agent::protocol::RpcError>) -> String {
    match result {
        Ok(answer) if answer["restart_required"] == true => {
            "Done. Restart the editor to apply it.".to_owned()
        }
        Ok(answer) => match answer["generation"].as_u64() {
            Some(generation) => format!("Reloaded (generation {generation})."),
            None => "Done.".to_owned(),
        },
        Err(error) => error.message,
    }
}

/// Extensions panel → a sample's Open: the copy goes into the Samples window's folder.
fn open_sample(world: &mut World, id: &str) -> String {
    let folder = world
        .get_resource_or_insert_with(crate::samples_browser::SamplesWindow::default)
        .folder
        .clone();
    let result = crate::samples_browser::create_and_open(
        world,
        &crate::samples_browser::Roots::detect(),
        id,
        std::path::Path::new(&folder),
        None,
        true,
    );
    crate::samples_browser::outcome_line(&result)
}

fn apply(world: &mut World, action: Action) {
    let message = match action {
        Action::Select(id) => {
            world.resource_mut::<ExtensionsPanelState>().selected = Some(id);
            return;
        }
        Action::Enable(id, grant) => outcome(regia_agent::plugins::enable(
            world,
            &json!({ "id": id, "grant": grant }),
        )),
        Action::Disable(id) => outcome(regia_agent::plugins::disable(world, &json!({ "id": id }))),
        Action::Reload(id) => outcome(super::host::reload(world, &json!({ "id": id }))),
        Action::OpenSample(id) => open_sample(world, &id),
    };
    world.resource_mut::<ExtensionsPanelState>().message = message;
}

/// Draws an extension's panel tab.
pub fn extension_tab(ui: &mut egui::Ui, world: &mut World, id: &str) {
    let draw = world
        .get_resource::<EditorExtensions>()
        .and_then(|editor| editor.panel(id))
        .map(|panel| std::sync::Arc::clone(&panel.draw));
    match draw {
        Some(draw) => draw(ui, world),
        None => {
            ui.weak("(this extension is not loaded)");
        }
    }
}

/// Which extension tabs to open (id and title, leaked once per panel; bounded by the panels
/// extensions register) and which of `open` to close, given the live registrations.
#[must_use]
pub fn tab_changes(
    world: &World,
    open: &[&'static str],
) -> (Vec<(&'static str, &'static str)>, Vec<&'static str>) {
    use regia_framework::extension::registry::RegistrationKind;
    let live: Vec<(&str, &str)> = match (
        world.get_resource::<EditorExtensions>(),
        world.get_resource::<ExtensionRegistry>(),
    ) {
        (Some(editor), Some(registry)) => editor
            .panels()
            .filter(|(owner, panel)| {
                registry.owner_of(RegistrationKind::EditorPanel, &panel.id) == Some(*owner)
            })
            .map(|(_, panel)| (panel.id.as_str(), panel.title_key.as_str()))
            .collect(),
        _ => Vec::new(),
    };
    let add = live
        .iter()
        .filter(|(id, _)| !open.contains(id))
        .map(|(id, title)| -> (&'static str, &'static str) {
            (
                Box::leak((*id).to_owned().into_boxed_str()),
                Box::leak((*title).to_owned().into_boxed_str()),
            )
        })
        .collect();
    let remove = open
        .iter()
        .filter(|id| !live.iter().any(|(live_id, _)| live_id == *id))
        .copied()
        .collect();
    (add, remove)
}

/// The customization an active extension registered for `type_path`, with its owner.
#[must_use]
pub fn inspector_for(world: &World, type_path: &str) -> Option<(String, InspectorDraw)> {
    use regia_framework::extension::registry::RegistrationKind;
    let editor = world.get_resource::<EditorExtensions>()?;
    let (owner, customization) = editor.inspector(type_path)?;
    let registry = world.get_resource::<ExtensionRegistry>()?;
    (registry.owner_of(RegistrationKind::Inspector, type_path) == Some(owner))
        .then(|| (owner.to_owned(), std::sync::Arc::clone(&customization.draw)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_needed_skip_implicit_and_granted() {
        let info = json!({
            "requested": ["systems", "agent_method", "process", "settings"],
            "granted": [
                { "capability": "systems", "grant": "implicit" },
                { "capability": "agent_method", "grant": "explicit" },
                { "capability": "settings", "grant": "implicit" },
            ],
        });
        assert_eq!(grants_needed(&info), ["process"]);
    }

    #[test]
    fn the_banner_never_promises_safety() {
        let text = TRUST_BANNER.to_lowercase();
        assert!(text.contains("not a sandbox"));
        assert!(!text.contains("sandboxed"));
        assert!(!text.contains(" safe"));
    }

    #[test]
    fn the_panel_draws_without_a_project() {
        let mut world = World::new();
        let context = egui::Context::default();
        let mut output = context.run_ui(egui::RawInput::default(), |ui| extensions_panel(ui, &mut world));
        output.textures_delta.clear();
    }
}
