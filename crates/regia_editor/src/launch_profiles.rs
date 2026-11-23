// SPDX-License-Identifier: MPL-2.0
//! Named project launch recipes and the Launch Profiles dialog (P-342, ADR-7843).

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use crate::viewport_agent::Handler;
pub use regia_asset_pipeline::launch_profiles::{FILE, FORMAT_VERSION, LaunchProfile, LaunchProfiles};

/// Launch Profiles dialog and the launch bar's selected profile.
#[derive(Resource, Default)]
pub struct ProfilesPanel {
    /// Whether the dialog is visible.
    pub open: bool,
    /// Selected saved recipe; `None` keeps the original launch bar behavior.
    pub selected: Option<String>,
    root: Option<PathBuf>,
    file: LaunchProfiles,
    draft: LaunchProfile,
    message: String,
    handled_frame: Option<u64>,
    draft_frame: Option<u64>,
}

fn err(code: &'static str, message: impl Into<String>) -> RpcError {
    crate::platforms_panel::error(code, message)
}

/// Reads the project's launch recipes. An absent file means an empty collection.
///
/// # Errors
/// IO, malformed data or an unsupported format version.
pub fn read(root: &Path) -> Result<LaunchProfiles, String> {
    let path = root.join(FILE);
    if let Ok(resolved) = path.canonicalize() {
        let project = root.canonicalize().map_err(|e| e.to_string())?;
        if !resolved.starts_with(project) {
            return Err("Launch profiles resolve outside the project".into());
        }
    }
    match std::fs::File::open(&path) {
        Ok(file) => {
            let mut text = String::new();
            file.take(4 * 1024 * 1024 + 1)
                .read_to_string(&mut text)
                .map_err(|e| e.to_string())?;
            LaunchProfiles::from_ron(&text)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LaunchProfiles::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn root(world: &World) -> Result<PathBuf, RpcError> {
    crate::mesh_source::project_root(world)
        .ok_or_else(|| err("launch_profiles_no_project", "Open a project first"))
}

fn write(world: &World, file: &LaunchProfiles) -> Result<(), RpcError> {
    let root = root(world)?;
    let path = crate::agent_ext::allowed_request_path(world, "path", &root.join(FILE).display().to_string())?;
    let text = file.to_ron().map_err(|e| err("launch_profiles_invalid", e))?;
    regia_asset_pipeline::loc_exchange::write_atomic(&path, &text)
        .map_err(|e| err("launch_profiles_io", e.to_string()))
}

fn sync(world: &mut World) {
    let root = crate::mesh_source::project_root(world);
    let mut panel = world.get_resource_or_insert_with(ProfilesPanel::default);
    if panel.root != root {
        panel.root = root.clone();
        panel.selected = None;
        panel.draft = LaunchProfile::default();
        match root.as_deref().map(read).transpose() {
            Ok(file) => {
                panel.file = file.unwrap_or_default();
                panel.message.clear();
            }
            Err(error) => {
                panel.file = LaunchProfiles::default();
                panel.message = error;
            }
        }
    }
}

/// The selected profile name. Resolving it at launch re-reads the project file.
#[must_use]
pub fn selected(world: &World) -> Option<String> {
    world
        .get_resource::<ProfilesPanel>()
        .and_then(|p| p.selected.clone())
}

// Cached display data; actual launches always resolve the file again.
pub(crate) fn display_selection(world: &World) -> Option<LaunchProfile> {
    let panel = world.get_resource::<ProfilesPanel>()?;
    panel
        .file
        .profiles
        .iter()
        .find(|p| Some(&p.name) == panel.selected.as_ref())
        .cloned()
}

/// Resolves a saved profile without caching stale settings from a prior launch.
///
/// # Errors
/// No project, unreadable/invalid file, or unknown name.
pub fn resolve(world: &World, name: &str) -> Result<LaunchProfile, RpcError> {
    read(&root(world)?)
        .map_err(|e| err("launch_profiles_invalid", e))?
        .profiles
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| {
            err(
                "launch_profile_not_found",
                format!("No launch profile named {name:?}"),
            )
        })
}

/// Draws the saved-profile dropdown and dialog button on the launch bar.
pub fn launch_bar(ui: &mut egui::Ui, world: &mut World) {
    sync(world);
    let mut panel = world.resource_mut::<ProfilesPanel>();
    let names: Vec<_> = panel.file.profiles.iter().map(|p| p.name.clone()).collect();
    let previous = panel.selected.clone();
    egui::ComboBox::from_label("Profile")
        .selected_text(panel.selected.as_deref().unwrap_or("Quick launch"))
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut panel.selected, None, "Quick launch");
            for name in names {
                ui.selectable_value(&mut panel.selected, Some(name.clone()), name);
            }
        });
    if panel.selected != previous
        && let Some(profile) = panel
            .file
            .profiles
            .iter()
            .find(|p| Some(&p.name) == panel.selected.as_ref())
            .cloned()
    {
        panel.draft = profile;
    }
    if crate::agent_parity::action_button(ui, &crate::agent_parity::LAUNCH_PROFILES_OPEN).clicked() {
        panel.open = true;
    }
    if !panel.message.is_empty() {
        ui.label(&panel.message);
    }
}

fn text(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.horizontal(|ui| {
        let name = ui.label(label);
        ui.text_edit_singleline(value).labelled_by(name.id);
    });
}

fn optional_text(ui: &mut egui::Ui, label: &str, value: &mut Option<String>) {
    let mut text_value = value.clone().unwrap_or_default();
    text(ui, label, &mut text_value);
    *value = (!text_value.is_empty()).then_some(text_value);
}

fn controls(ui: &mut egui::Ui, panel: &mut ProfilesPanel) -> (egui::Response, bool) {
    let frame = ui.ctx().cumulative_frame_nr();
    let fresh = panel.draft_frame != Some(frame);
    let mut draft_action = false;
    egui::ComboBox::from_label("Saved profiles")
        .selected_text(panel.selected.as_deref().unwrap_or("New profile"))
        .show_ui(ui, |ui| {
            for profile in &panel.file.profiles {
                if ui
                    .selectable_value(&mut panel.selected, Some(profile.name.clone()), &profile.name)
                    .changed()
                {
                    panel.draft = profile.clone();
                }
            }
        });
    if crate::agent_parity::action_button(ui, &crate::agent_parity::LAUNCH_PROFILE_NEW).clicked() && fresh {
        draft_action = true;
        panel.selected = None;
        panel.draft = LaunchProfile::default();
    }
    let draft = &mut panel.draft;
    text(ui, "Name", &mut draft.name);
    egui::ComboBox::from_label("Target")
        .selected_text(&draft.target)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut draft.target, "standalone".into(), "Standalone");
            ui.selectable_value(&mut draft.target, "android".into(), "Android device");
        });
    text(ui, "Build profile", &mut draft.build_profile);
    egui::ComboBox::from_label("Cook")
        .selected_text(&draft.cook)
        .show_ui(ui, |ui| {
            for choice in ["incremental", "full", "skip"] {
                ui.selectable_value(&mut draft.cook, choice.into(), choice);
            }
        });
    crate::agent_parity::action_toggle(
        ui,
        &crate::agent_parity::LAUNCH_PROFILE_PACKAGE,
        &mut draft.package,
    );
    ui.weak("Packaging may cook content required by the selected build profile.");
    optional_text(ui, "Deploy device (blank: only device)", &mut draft.device);
    optional_text(ui, "Start level (under assets/)", &mut draft.start_level);
    ui.label("Extra arguments (one field per argument)");
    let mut remove = None;
    for (index, argument) in draft.extra_arguments.iter_mut().enumerate() {
        ui.push_id(index, |ui| {
            ui.horizontal(|ui| {
                text(ui, &format!("Argument {}", index + 1), argument);
                if crate::agent_parity::action_button(ui, &crate::agent_parity::LAUNCH_PROFILE_REMOVE_ARG)
                    .clicked()
                {
                    remove = Some(index);
                }
            });
        });
    }
    if let Some(index) = remove
        && fresh
    {
        draft_action = true;
        draft.extra_arguments.remove(index);
    }
    if crate::agent_parity::action_button(ui, &crate::agent_parity::LAUNCH_PROFILE_ADD_ARG).clicked() && fresh
    {
        draft_action = true;
        draft.extra_arguments.push(String::new());
    }
    if draft.target == "android" {
        ui.weak(
            "Android always packages the APK; the start level and arguments reach the device game in the launch intent.",
        );
    }
    let save = crate::agent_parity::action_button(ui, &crate::agent_parity::LAUNCH_PROFILE_SAVE);
    let delete = crate::agent_parity::action_enabled(
        ui,
        panel.selected.is_some(),
        &crate::agent_parity::LAUNCH_PROFILE_DELETE,
        None,
    )
    .clicked();
    if !panel.message.is_empty() {
        ui.label(&panel.message);
    }
    if draft_action {
        panel.draft_frame = Some(frame);
    }
    (save, delete)
}

/// Draws the launch profiles window and applies each save/delete at most once per egui frame.
#[tracing::instrument(skip_all)]
pub fn show(ctx: &egui::Context, world: &mut World) {
    sync(world);
    let mut panel = world.remove_resource::<ProfilesPanel>().unwrap_or_default();
    let mut open = panel.open;
    let mut request = None;
    egui::Window::new("Launch Profiles")
        .open(&mut open)
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                let (save, delete) = controls(ui, &mut panel);
                if save.clicked() {
                    request = Some(json!({"op":"save","profile":panel.draft}));
                } else if delete {
                    request = Some(json!({"op":"delete","name":panel.selected}));
                }
            });
        });
    panel.open = open;
    let fresh = panel.handled_frame != Some(ctx.cumulative_frame_nr());
    if request.is_some() && fresh {
        panel.handled_frame = Some(ctx.cumulative_frame_nr());
    }
    world.insert_resource(panel);
    if fresh && let Some(request) = request {
        let message = match launch_profiles(world, &request) {
            Ok(_) => "Launch profiles saved".into(),
            Err(e) => e.message,
        };
        world.resource_mut::<ProfilesPanel>().message = message;
    }
}

fn launch_profiles(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    sync(world);
    if let Some(open) = params.get("open") {
        let open = open
            .as_bool()
            .ok_or_else(|| err("launch_profiles_invalid", "open must be boolean"))?;
        world.resource_mut::<ProfilesPanel>().open = open;
    }
    let root = root(world)?;
    let mut file = read(&root).map_err(|e| err("launch_profiles_invalid", e))?;
    let op = match params.get("op") {
        None => "list",
        Some(Value::String(op)) => op,
        _ => return Err(err("launch_profiles_bad_op", "op must be a string")),
    };
    match op {
        "list" => {}
        "save" => {
            let profile: LaunchProfile = serde_json::from_value(
                params
                    .get("profile")
                    .cloned()
                    .ok_or_else(|| err("launch_profiles_invalid", "profile is required"))?,
            )
            .map_err(|e| err("launch_profiles_invalid", e.to_string()))?;
            profile
                .validate()
                .map_err(|e| err("launch_profiles_invalid", e))?;
            if let Some(old) = file.profiles.iter_mut().find(|p| p.name == profile.name) {
                *old = profile.clone();
            } else {
                file.profiles.push(profile.clone());
            }
            write(world, &file)?;
            let mut panel = world.resource_mut::<ProfilesPanel>();
            panel.selected = Some(profile.name.clone());
            panel.draft = profile;
        }
        "delete" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| err("launch_profiles_invalid", "name is required"))?;
            let before = file.profiles.len();
            file.profiles.retain(|p| p.name != name);
            if before == file.profiles.len() {
                return Err(err(
                    "launch_profile_not_found",
                    format!("No launch profile named {name:?}"),
                ));
            }
            write(world, &file)?;
            let mut panel = world.resource_mut::<ProfilesPanel>();
            if panel.selected.as_deref() == Some(name) {
                panel.selected = None;
            }
        }
        _ => return Err(err("launch_profiles_bad_op", "op is list, save or delete")),
    }
    world.resource_mut::<ProfilesPanel>().file = file.clone();
    Ok(json!(file))
}

/// Profile management route and schema.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.launch_profiles",
            tool: "launch_profiles",
            description: "List, save or delete named project launch profiles in config/launch_profiles.ron v1. Save replaces the exact name atomically. Errors: launch_profiles_no_project, launch_profiles_invalid, launch_profiles_io, launch_profile_not_found, launch_profiles_bad_op.",
            params: &[
                ParamSchema {
                    name: "op",
                    kind: "string",
                    required: false,
                    description: "list (default), save or delete.",
                },
                ParamSchema {
                    name: "profile",
                    kind: "object",
                    required: false,
                    description: "For save: name, target, build_profile, cook, package, device, start_level, extra_arguments.",
                },
                ParamSchema {
                    name: "name",
                    kind: "string",
                    required: false,
                    description: "Exact profile name to delete.",
                },
                ParamSchema {
                    name: "open",
                    kind: "boolean",
                    required: false,
                    description: "Show or hide Launch Profiles.",
                },
            ],
            result: "{ format_version, profiles }",
            mutating: true,
            permission: Permission::Assets,
        },
        launch_profiles as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_profiles_save_is_keyboard_operable() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        for scale in [1.0, 1.5] {
            let ctx = egui::Context::default();
            ctx.set_zoom_factor(scale);
            let mut panel = ProfilesPanel::default();
            let mut activated = false;
            let mut draw = |ui: &mut egui::Ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    activated |= controls(ui, &mut panel).0.clicked();
                });
            };
            frame(&ctx, [1000.0, 1200.0], vec![], &mut draw);
            tab_to(&ctx, [1000.0, 1200.0], "Save profile", false, 60, &mut draw);
            frame(
                &ctx,
                [1000.0, 1200.0],
                vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
                &mut draw,
            );
            assert!(
                activated,
                "Tab reaches and Enter activates Save profile at scale {scale}"
            );
        }
    }
}
