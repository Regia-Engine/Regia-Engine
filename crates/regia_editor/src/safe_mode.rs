// SPDX-License-Identifier: MPL-2.0
//! Recovery startup policy and its banner. Startup markers are empty process bookkeeping
//! files in the user config directory, never project content or a versioned document format.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

/// Recovery startup state shared by the host, shell and agent.
#[derive(Resource, Clone, Debug, Default)]
pub struct SafeMode {
    /// Skip project code, restores and agent autostart for this process.
    pub enabled: bool,
    /// Explain two unclean exits and offer a normal restart.
    pub startup_prompt: bool,
    /// The user requested a normal restart after the normal unsaved-document close flow.
    pub restart_requested: bool,
    acted_frame: Option<u64>,
}

impl SafeMode {
    /// Explicit safe mode or recovery after two unclean exits.
    #[must_use]
    pub fn new(explicit: bool, repeated_failure: bool) -> Self {
        Self {
            enabled: explicit || repeated_failure,
            startup_prompt: repeated_failure,
            ..Self::default()
        }
    }
}

/// Whether this editor process uses recovery startup policy.
#[must_use]
pub fn enabled(world: &World) -> bool {
    world
        .get_resource::<SafeMode>()
        .is_some_and(|state| state.enabled)
}

/// Serializes startup attempts so another concurrently starting editor does not count as a crash.
#[derive(Resource)]
pub struct StartupAttempt {
    _lock: File,
    directory: PathBuf,
}

impl StartupAttempt {
    /// Records this session. A prior process that failed before orderly teardown leaves
    /// a marker behind. A live editor holds the file lock and is never counted as a crash.
    ///
    /// # Errors
    /// The user directory cannot be written or another editor still holds the startup lock.
    pub fn begin(directory: &Path) -> std::io::Result<(Self, bool)> {
        std::fs::create_dir_all(directory)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("editor-startup.lock"))?;
        lock.try_lock().map_err(std::io::Error::other)?;
        let first = directory.join("editor-startup-1");
        let second = directory.join("editor-startup-2");
        let repeated = first.exists() && second.exists();
        let next = if first.exists() { &second } else { &first };
        match OpenOptions::new().write(true).create_new(true).open(next) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        Ok((
            Self {
                _lock: lock,
                directory: directory.to_owned(),
            },
            repeated,
        ))
    }

    /// Clears crash markers after successful, orderly application teardown.
    ///
    /// # Errors
    /// A marker cannot be removed. Missing markers are harmless.
    pub fn finish_cleanly(self) -> std::io::Result<()> {
        for name in ["editor-startup-1", "editor-startup-2"] {
            match std::fs::remove_file(self.directory.join(name)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

/// Clears session markers after the host has dropped its application successfully.
pub fn clean_exit(attempt: Option<StartupAttempt>) {
    if let Some(attempt) = attempt
        && let Err(error) = attempt.finish_cleanly()
    {
        tracing::warn!(target: "regia_editor", %error, "could not clear startup markers");
    }
}

/// Draws the recovery banner and, after two unclean exits, a keyboard-accessible choice.
/// Side effects are guarded against egui's repeated layout passes.
pub fn banner(ui: &mut egui::Ui, world: &mut World) {
    let ctx = ui.ctx().clone();
    if !enabled(world) {
        return;
    }
    let mut restart = false;
    let mut continue_safe = false;
    egui::Panel::top("editor-safe-mode").show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label("Safe mode: extensions, Lua mods and automatic restores are disabled.");
            restart =
                crate::agent_parity::action_button(ui, &crate::agent_parity::SAFE_MODE_RESTART).clicked();
        });
    });
    if world.resource::<SafeMode>().startup_prompt {
        egui::Window::new("Startup recovery")
            .collapsible(false)
            .show(&ctx, |ui| {
                ui.label("The last two editor sessions ended unexpectedly. The editor started in safe mode.");
                continue_safe =
                    crate::agent_parity::action_button(ui, &crate::agent_parity::SAFE_MODE_CONTINUE)
                        .clicked();
            });
    }
    let mut state = world.resource_mut::<SafeMode>();
    let frame = ctx.cumulative_frame_nr();
    if state.acted_frame != Some(frame) && (restart || continue_safe) {
        state.acted_frame = Some(frame);
        state.restart_requested |= restart;
        state.startup_prompt = false;
    }
}

fn state(world: &mut World, _: &Value) -> Result<Value, RpcError> {
    let state = world.get_resource::<SafeMode>().cloned().unwrap_or_default();
    Ok(
        json!({"enabled": state.enabled, "startup_prompt": state.startup_prompt, "restart_requested": state.restart_requested,
        "skipped": if state.enabled { vec!["extensions", "lua_mods", "autosave_restore", "level_restore", "tab_restore", "agent_autostart"] } else { vec![] }}),
    )
}

fn restart(world: &mut World, _: &Value) -> Result<Value, RpcError> {
    if !enabled(world) {
        let mut error = RpcError::invalid_params("normal restart requires safe mode");
        if let Some(data) = &mut error.data {
            data["reason"] = json!("safe_mode_inactive");
        }
        return Err(error);
    }
    world.resource_mut::<SafeMode>().restart_requested = true;
    state(world, &Value::Null)
}

/// Recovery state and the banner's restart action.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, crate::viewport_agent::Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.safe_mode_state",
                tool: "editor_safe_mode_state",
                description: "Recovery startup policy and pending normal restart. Safe mode never autostarts an agent listener.",
                params: &[],
                result: "{ enabled, startup_prompt, restart_requested, skipped }",
                mutating: false,
                permission: Permission::Read,
            },
            state,
        ),
        (
            MethodSchema {
                name: "editor.safe_mode_restart",
                tool: "editor_safe_mode_restart",
                description: "Request a normal restart through the editor's unsaved-document close flow. Refuses safe_mode_inactive.",
                params: &[],
                result: "{ enabled, startup_prompt, restart_requested, skipped }",
                mutating: true,
                permission: Permission::Tools,
            },
            restart,
        ),
    ]
}

/// Arguments for the normal restart, preserving project and game selection.
#[must_use]
pub fn normal_arguments(arguments: impl IntoIterator<Item = std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let mut arguments: Vec<_> = arguments
        .into_iter()
        .filter(|arg| arg != "--safe-mode" && arg != "--normal-mode")
        .collect();
    arguments.push("--normal-mode".into());
    arguments
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_unclean_sessions_prompt_and_clean_exit_resets_the_sequence() {
        let dir = regia_test_utils::unique_temp_dir("safe-startup");
        let (first, prompt) = StartupAttempt::begin(&dir).expect("first");
        assert!(!prompt);
        assert!(
            StartupAttempt::begin(&dir).is_err(),
            "a live start holds the lock"
        );
        drop(first); // Simulate an unclean process exit.
        let (second, prompt) = StartupAttempt::begin(&dir).expect("second");
        assert!(!prompt);
        drop(second);
        let (third, prompt) = StartupAttempt::begin(&dir).expect("third");
        assert!(prompt);
        third.finish_cleanly().expect("successful startup");
        let (next, prompt) = StartupAttempt::begin(&dir).expect("next");
        assert!(!prompt);
        next.finish_cleanly().expect("ready");
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn normal_restart_preserves_project_arguments_without_the_safe_flag() {
        let args = normal_arguments(
            ["--safe-mode", "--project", "project with spaces", "--normal-mode"].map(Into::into),
        );
        assert_eq!(
            args,
            ["--project", "project with spaces", "--normal-mode"].map(std::ffi::OsString::from)
        );
    }
}

#[cfg(test)]
mod keyboard_tests {
    use super::*;
    use crate::canvas_keys::test_input::{frame, key, tab_to};

    #[test]
    fn recovery_controls_are_named_and_operable_from_the_keyboard_at_two_scales() {
        for scale in [1.0, 1.75] {
            let context = egui::Context::default();
            context.set_zoom_factor(scale);
            let mut world = World::new();
            world.insert_resource(SafeMode::new(false, true));
            frame(&context, [900.0, 600.0], vec![], |ui| banner(ui, &mut world));
            tab_to(
                &context,
                [900.0, 600.0],
                "Continue in safe mode",
                false,
                12,
                |ui| banner(ui, &mut world),
            );
            frame(
                &context,
                [900.0, 600.0],
                vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
                |ui| banner(ui, &mut world),
            );
            assert!(!world.resource::<SafeMode>().startup_prompt);
            assert!(!world.resource::<SafeMode>().restart_requested);
            tab_to(&context, [900.0, 600.0], "Restart normally", false, 12, |ui| {
                banner(ui, &mut world)
            });
            frame(
                &context,
                [900.0, 600.0],
                vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
                |ui| banner(ui, &mut world),
            );
            assert!(world.resource::<SafeMode>().restart_requested);
        }
    }
}
