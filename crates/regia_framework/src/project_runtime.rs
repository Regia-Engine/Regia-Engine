// SPDX-License-Identifier: MPL-2.0
//! The `[runtime]` table of `Regia.toml` (`docs/specs/11` §§ 2-3, ADR-7070): project settings the
//! running game applies at start-up.
//!
//! ```toml
//! [runtime]
//! pause_on_focus_loss = false   # pause gameplay time while the window is out of focus
//! keep_screen_on = false        # keep the display awake while the game runs (Android)
//! ```
//!
//! Both keys are optional and default to `false`, so older manifests read as before (no version
//! bump; `docs/formats/regia-toml-runtime.md`). [`ProjectRuntimePlugin`] reads the table once and
//! turns on `regia_platform::PauseOnFocusLoss` and `regia_platform::KeepScreenOn` for a `true` key; a
//! `false` key leaves a value the game set in code alone. `regia::DefaultPlugins` adds it.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_platform::{KeepScreenOn, PauseOnFocusLoss};
use serde::Deserialize;

/// The `[runtime]` table of `Regia.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct ProjectRuntimeConfig {
    /// Pause gameplay `Time` while the primary window is out of focus.
    pub pause_on_focus_loss: bool,
    /// Keep the screen on while the game runs (Android `FLAG_KEEP_SCREEN_ON`; no effect on desktop).
    pub keep_screen_on: bool,
}

#[derive(Debug, Default, Deserialize)]
struct Manifest {
    #[serde(default)]
    runtime: ProjectRuntimeConfig,
}

impl ProjectRuntimeConfig {
    /// Parses the `[runtime]` table out of a whole `Regia.toml`; an absent table is the default. A key
    /// of the wrong type is an error naming the table.
    pub fn from_manifest(text: &str) -> Result<Self, String> {
        toml::from_str::<Manifest>(text)
            .map(|m| m.runtime)
            .map_err(|e| format!("Regia.toml [runtime]: {e}"))
    }

    /// Reads the project's manifest under `root`; a missing manifest is the default.
    pub fn load(root: &Path) -> Result<Self, String> {
        let Some(path) = regia_core::brand::project_manifest(root) else {
            return Ok(Self::default());
        };
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::from_manifest(&text)
    }

    /// Applies the table to `world`: a `true` key turns its resource on; a `false` key inserts the
    /// default only where the resource is missing.
    pub fn apply(self, world: &mut World) {
        if self.pause_on_focus_loss || !world.contains_resource::<PauseOnFocusLoss>() {
            world.insert_resource(PauseOnFocusLoss(self.pause_on_focus_loss));
        }
        if self.keep_screen_on || !world.contains_resource::<KeepScreenOn>() {
            world.insert_resource(KeepScreenOn(self.keep_screen_on));
        }
    }
}

/// The settings the running game reads from `Regia.toml` (`[runtime]`), stored for the agent's
/// `settings.get`.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProjectRuntime(pub ProjectRuntimeConfig);

/// Reads `[runtime]` once at build time and applies it (see the module docs).
#[derive(Debug, Clone, Default)]
pub struct ProjectRuntimePlugin {
    /// The project root; `None` detects it (`$REGIA_PROJECT` or the nearest `Regia.toml`).
    pub project_root: Option<PathBuf>,
    /// Use this table instead of reading the manifest.
    pub config: Option<ProjectRuntimeConfig>,
}

impl Plugin for ProjectRuntimePlugin {
    fn build(&self, app: &mut App) {
        let config = match self.config {
            Some(config) => config,
            None => {
                let root = self
                    .project_root
                    .clone()
                    .unwrap_or_else(regia_asset::detect_project_root);
                ProjectRuntimeConfig::load(&root).unwrap_or_else(|e| {
                    tracing::warn!(target: "regia_framework", code = "runtime_settings_invalid", "{e}");
                    ProjectRuntimeConfig::default()
                })
            }
        };
        config.apply(app.world_mut());
        app.insert_resource(ProjectRuntime(config));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_false_key_keeps_a_value_set_in_code() {
        let mut world = World::new();
        world.insert_resource(KeepScreenOn(true));
        ProjectRuntimeConfig::default().apply(&mut world);
        assert_eq!(world.resource::<KeepScreenOn>(), &KeepScreenOn(true));
        assert_eq!(world.resource::<PauseOnFocusLoss>(), &PauseOnFocusLoss(false));
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let config =
            ProjectRuntimeConfig::from_manifest("[runtime]\nkeep_screen_on = true\nfuture_key = 3\n")
                .expect("unknown keys are tolerated");
        assert!(config.keep_screen_on);
    }
}
