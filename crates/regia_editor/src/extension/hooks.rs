// SPDX-License-Identifier: MPL-2.0
//! Build and launch hooks extensions register (ADR-7791 (5), spec addendum § 20.3).
//!
//! A hook runs at one [`HookStage`]: before the cook of File ▸ Package for platform (the package
//! job's first step), after a package run published its output, or before a launch starts. It gets
//! a [`HookInput`] (project, target, profile, output folder) and a [`HookOutput`] to fill: files to
//! add (project-relative before the cook and the launch, output-relative after a package; a path
//! that leaves its folder is refused), log lines the job shows, and `Err(message)` to abort.
//!
//! Hooks take no world: the after-package stage runs when the package job is noticed finished, and
//! a hook must not depend on what the editor shows at that moment. Hooks of one stage run in id
//! order; the first abort stops the stage. A panic in a hook aborts the stage with a message and
//! the editor keeps running.
//!
//! Where they run: `package_job::start_package` (before cook: an abort refuses the run with
//! `package_hook_aborted`), `package_job::sync_tasks` (after package: an abort turns the finished
//! job's result into `ok: false, code: package_hook_aborted`), and `launch::launch` (before launch:
//! an abort refuses with `launch_hook_aborted`). The agent surface is `editor.extension_ui`
//! (`hooks: [{ id, title, stage, owner }]`, `book/agents/editor-extensions.md`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_ecs::prelude::*;
use regia_framework::extension::registry::RegistrationKind;

use super::context::EditorExtensions;

/// When a hook runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HookStage {
    /// Before File ▸ Package for platform starts its cook; files land in the project.
    BeforeCook,
    /// After a package run moved its output into place; files land in the output folder.
    AfterPackage,
    /// Before a launch builds and starts the game; files land in the project.
    BeforeLaunch,
}

impl HookStage {
    /// The lowercase name the agent surface uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::BeforeCook => "before_cook",
            Self::AfterPackage => "after_package",
            Self::BeforeLaunch => "before_launch",
        }
    }
}

/// What a hook is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookInput {
    /// The stage.
    pub stage: HookStage,
    /// The project folder.
    pub project: PathBuf,
    /// `linux`, `windows`, `android` (packages) or `standalone`, `android` (launches).
    pub target: String,
    /// The package profile (`dev`, `shipping`, ...); empty for a launch.
    pub profile: String,
    /// The package output folder (after package only).
    pub out: Option<PathBuf>,
}

impl HookInput {
    /// The folder [`HookOutput::files`] are relative to: the output folder after a package, else
    /// the project.
    #[must_use]
    pub fn files_root(&self) -> &Path {
        match (&self.stage, &self.out) {
            (HookStage::AfterPackage, Some(out)) => out,
            _ => &self.project,
        }
    }
}

/// What a hook hands back besides its abort.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookOutput {
    /// Files to write: a relative path (forward slashes) under [`HookInput::files_root`], and the
    /// bytes.
    pub files: Vec<(String, Vec<u8>)>,
    /// Lines the job shows.
    pub log: Vec<String>,
}

/// Runs a hook; `Err` aborts the stage with that message.
pub type HookRun = Arc<dyn Fn(&HookInput, &mut HookOutput) -> Result<(), String> + Send + Sync>;

/// A build or launch hook an extension registers.
#[derive(Clone)]
pub struct BuildHookDesc {
    /// `<ext>.<hook>`.
    pub id: String,
    /// A short name for the job log.
    pub title: String,
    /// When it runs.
    pub stage: HookStage,
    /// The callback.
    pub run: HookRun,
}

/// What a stage did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookReport {
    /// Log lines, each prefixed with the hook id.
    pub log: Vec<String>,
    /// Files written, relative to the stage's folder.
    pub files: Vec<String>,
    /// The abort message (`<hook id>: <message>`), when a hook aborted.
    pub aborted: Option<String>,
}

/// The live hooks of `stage`, `(owner, hook)` in id order.
#[must_use]
pub fn hooks(world: &World, stage: HookStage) -> Vec<(String, BuildHookDesc)> {
    all(world)
        .into_iter()
        .filter(|(_, hook)| hook.stage == stage)
        .collect()
}

/// Every live hook, `(owner, hook)` in id order.
#[must_use]
pub fn all(world: &World) -> Vec<(String, BuildHookDesc)> {
    let (Some(editor), Some(registry)) = (
        world.get_resource::<EditorExtensions>(),
        world.get_resource::<regia_framework::extension::ExtensionRegistry>(),
    ) else {
        return Vec::new();
    };
    editor
        .hooks
        .iter()
        .filter(|(name, (owner, _))| {
            registry.owner_of(RegistrationKind::BuildHook, name) == Some(owner.as_str())
        })
        .map(|(_, (owner, hook))| (owner.clone(), hook.clone()))
        .collect()
}

/// Runs the live hooks of `input.stage` in id order and writes the files they add. Stops at the
/// first abort (a refused file path or a failed write aborts too).
#[must_use]
pub fn run_stage(world: &World, input: &HookInput) -> HookReport {
    let list = hooks(world, input.stage);
    run_hooks(&list, input)
}

/// Runs `list` (already filtered to `input.stage`); see [`run_stage`].
#[must_use]
pub fn run_hooks(list: &[(String, BuildHookDesc)], input: &HookInput) -> HookReport {
    let mut report = HookReport::default();
    if list.is_empty() {
        return report;
    }
    let _span = tracing::info_span!("extension_hooks", stage = input.stage.name()).entered();
    for (_, hook) in list {
        let mut output = HookOutput::default();
        let run = Arc::clone(&hook.run);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(input, &mut output)))
            .unwrap_or_else(|_| Err("the hook panicked".to_owned()));
        report
            .log
            .extend(output.log.iter().map(|line| format!("{}: {line}", hook.id)));
        if let Err(message) = result {
            report.aborted = Some(format!("{}: {message}", hook.id));
            return report;
        }
        for (path, bytes) in &output.files {
            if let Err(message) = write_file(input.files_root(), path, bytes) {
                report.aborted = Some(format!("{}: {message}", hook.id));
                return report;
            }
            report.files.push(path.replace('\\', "/"));
        }
    }
    report
}

fn write_file(root: &Path, path: &str, bytes: &[u8]) -> Result<(), String> {
    let relative =
        regia_core::project_path::check_relative(path).map_err(|error| format!("`{path}`: {error}"))?;
    let file = root.join(relative);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("`{path}`: {error}"))?;
    }
    crate::level_io::write_atomic(&file, bytes).map_err(|error| format!("`{path}`: {error}"))
}
