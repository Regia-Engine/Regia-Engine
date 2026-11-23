// SPDX-License-Identifier: MPL-2.0
//! Cancellable background tasks (`docs/specs/19-editor.md` § 7 "long operations run in background
//! with progress notifications and cancel", ADR-9770): the game build and content import.
//!
//! The game build publishes a row in [`BackgroundTasks`] with the [`CancelToken`] it polls; the
//! toolbar's Cancel button and the agent's `editor.cancel_task { id: "game_build" }` set it, as
//! does `editor.cancel_job { id: "build" }` (ADR-7205). `editor.cancel_task { id: "import" }`
//! stops the Content Browser's background import job (ADR-7205's `ContentImport`; the import
//! removes what it had copied). ADR-7381 merged the two lanes onto these single mechanisms.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use regia_ecs::prelude::*;

/// The id of the game build task.
pub const GAME_BUILD: &str = "game_build";
/// The id of the content import task.
pub const IMPORT: &str = "import";

/// A shared cancel flag.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A fresh, not cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the task to stop.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether cancel was asked.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// One running task.
#[derive(Debug, Clone)]
pub struct TaskRow {
    /// [`GAME_BUILD`] or [`IMPORT`].
    pub id: String,
    /// What the progress notification says.
    pub label: String,
    /// Done fraction, when known.
    pub progress: Option<f32>,
    /// The task's cancel flag.
    pub cancel: CancelToken,
}

/// The running background tasks (an edit-world resource).
#[derive(Resource, Debug, Default, Clone)]
pub struct BackgroundTasks {
    rows: Vec<TaskRow>,
}

impl BackgroundTasks {
    /// Adds or replaces the row of `row.id`.
    pub fn publish(&mut self, row: TaskRow) {
        self.rows.retain(|r| r.id != row.id);
        self.rows.push(row);
    }

    /// Removes the row of `id`.
    pub fn finish(&mut self, id: &str) {
        self.rows.retain(|r| r.id != id);
    }

    /// The running tasks.
    #[must_use]
    pub fn rows(&self) -> &[TaskRow] {
        &self.rows
    }

    /// Cancels `id`. `false` when no such task runs.
    pub fn cancel(&self, id: &str) -> bool {
        match self.rows.iter().find(|r| r.id == id) {
            Some(row) => {
                row.cancel.cancel();
                true
            }
            None => false,
        }
    }
}

/// Cancels the task `id` of the world (`editor.cancel_task`, the Cancel buttons). `false` when
/// no such task runs. [`IMPORT`] stops the Content Browser's background import
/// ([`crate::editor_jobs::ContentImport`], the job `editor.cancel_job` also stops; ADR-7381).
pub fn cancel(world: &World, id: &str) -> bool {
    if id == IMPORT
        && let Some(import) = world.get_resource::<crate::editor_jobs::ContentImport>()
        && import.0.running()
    {
        import.0.cancel();
        return true;
    }
    world
        .get_resource::<BackgroundTasks>()
        .is_some_and(|t| t.cancel(id))
}

/// Publishes (or clears) the game build's row from the hot reload host's state each frame.
pub fn sync_game_build(world: &mut World, running: Option<(CancelToken, f32)>) {
    let mut tasks = world.get_resource_or_insert_with(BackgroundTasks::default);
    match running {
        Some((cancel, seconds)) => tasks.publish(TaskRow {
            id: GAME_BUILD.to_string(),
            label: format!("compiling game code ({seconds:.0} s)"),
            progress: None,
            cancel,
        }),
        None => tasks.finish(GAME_BUILD),
    }
}

/// A Cancel button for task `id` when it runs (a progress notification's button). Returns
/// whether it was clicked (the task is cancelled then). Agent method: `editor.cancel_task`.
pub fn cancel_button(ui: &mut egui::Ui, world: &World, id: &str) -> bool {
    let running = world
        .get_resource::<BackgroundTasks>()
        .is_some_and(|t| t.rows.iter().any(|r| r.id == id));
    if running && ui.small_button("Cancel").clicked() {
        return cancel(world, id);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_task_sets_the_shared_token() {
        let mut world = World::new();
        let token = CancelToken::new();
        sync_game_build(&mut world, Some((token.clone(), 1.0)));
        assert!(!cancel(&world, IMPORT), "no import runs");
        assert!(cancel(&world, GAME_BUILD));
        assert!(token.is_cancelled());
        sync_game_build(&mut world, None);
        assert!(world.resource::<BackgroundTasks>().rows().is_empty());
    }
}
