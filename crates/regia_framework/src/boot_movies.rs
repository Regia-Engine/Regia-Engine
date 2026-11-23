// SPDX-License-Identifier: MPL-2.0
//! Startup movies from the project file (`docs/specs/50-video-playback.md` § 2.7, ADR-2410).
//!
//! `Regia.toml` may list movies played before the first level:
//!
//! ```toml
//! [boot]
//! movies = ["movies/studio.video.ron", "movies/intro.webm"]
//! skippable_after = 1.5   # seconds of each movie before skip_video works (default 1.0)
//! ```
//!
//! Paths are project paths, resolved by `regia_media::MediaFiles` under the project root. An absent
//! table or an empty list means no boot movies (the key is additive: no format bump). A table that
//! does not parse is logged and ignored, so a typo never blocks the game from starting.
//!
//! Without a loose manifest, packaged build info supplies the list. Desktop package paths are
//! relative to the staged `assets/` directory; Android keeps the extracted media root. Explicit
//! code configuration takes precedence over either file (ADR-7893).
//!
//! [`BootMoviesPlugin`] (after `regia_media::MediaPlugin`) reads the list once, inserts
//! `regia_media::BootMovies`, adds the default [`regia_input::SKIP_VIDEO_ACTION`] bindings
//! (`InputConfig::with_skip_video_default`: Escape, Space, Enter, gamepad South and Start) unless the
//! project binds the action itself, enables the consuming [`regia_input::VIDEO_CONTEXT`] for player 0
//! while a boot movie or a skippable fullscreen video plays, and turns each press into a
//! `regia_media::SkipVideo`. Movies play fullscreen above the UI while level loading continues on
//! other threads; a level reveal waits on the run condition `regia_media::boot_movies_done`.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_input::{Actions, InputConfig, InputContexts, SKIP_VIDEO_ACTION, VIDEO_CONTEXT};
use regia_media::boot::{BootMovies, DEFAULT_SKIPPABLE_AFTER, SkipVideo};
use regia_media::plugin::MediaFiles;
use regia_media::{VideoFullscreen, VideoPlayerState};
use serde::Deserialize;

/// The `[boot]` table of `Regia.toml`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct BootConfig {
    /// Movies in play order.
    pub movies: Vec<String>,
    /// Seconds of each movie before it may be skipped.
    pub skippable_after: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct Manifest {
    #[serde(default)]
    boot: BootConfig,
}

impl BootConfig {
    /// Parses the `[boot]` table out of a whole `Regia.toml`.
    pub fn from_manifest(text: &str) -> Result<Self, String> {
        toml::from_str::<Manifest>(text)
            .map(|m| m.boot)
            .map_err(|e| format!("Regia.toml [boot]: {e}"))
    }

    /// Reads a loose manifest under `root`, else the boot object in that directory's packaged
    /// `build_info.json`. Missing metadata means no movies; malformed metadata returns an error.
    pub fn load(root: &Path) -> Result<Self, String> {
        if let Some(path) = regia_core::brand::project_manifest(root) {
            return Self::read_manifest(&path);
        }
        Self::from_package(&crate::packaged_config::PackagedConfig::read_directory(root))
            .map(Option::unwrap_or_default)
    }

    fn read_manifest(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::from_manifest(&text)
    }

    fn from_package(package: &crate::packaged_config::PackagedConfig) -> Result<Option<Self>, String> {
        package
            .boot()
            .map_err(str::to_string)?
            .map(|value| {
                serde_json::from_value(value.clone()).map_err(|e| format!("build_info.json boot: {e}"))
            })
            .transpose()
    }

    /// The sequence to play.
    pub fn sequence(&self) -> BootMovies {
        BootMovies::new(self.movies.iter().cloned())
            .skippable_after(self.skippable_after.unwrap_or(DEFAULT_SKIPPABLE_AFTER))
    }
}

/// Origin of the startup boot list, including an empty or invalid configuration (ADR-7893).
#[derive(Resource, Debug, Clone)]
pub struct BootMoviesOrigin {
    /// `code`, `manifest`, `packaged`, `default`, or `invalid`.
    pub source: &'static str,
    /// Manifest/build-report location; absent for code or a missing configuration.
    pub location: Option<String>,
}

/// Plays the project's boot movies and maps the `skip_video` action (see the module docs).
#[derive(Debug, Clone, Default)]
pub struct BootMoviesPlugin {
    /// The project root; `None` detects it (`$REGIA_PROJECT` or the nearest `Regia.toml`).
    pub project_root: Option<PathBuf>,
    /// Use this list instead of reading a loose manifest or package metadata.
    pub config: Option<BootConfig>,
}

impl Plugin for BootMoviesPlugin {
    fn build(&self, app: &mut App) {
        let root = self
            .project_root
            .clone()
            .unwrap_or_else(regia_asset::detect_project_root);
        let mut origin = BootMoviesOrigin {
            source: "default",
            location: None,
        };
        let mut media_root = root.clone();
        let result = if app.world().contains_resource::<BootMovies>() {
            origin.source = "code";
            Ok(BootConfig::default())
        } else if let Some(config) = &self.config {
            origin.source = "code";
            Ok(config.clone())
        } else if let Some(path) = regia_core::brand::project_manifest(&root) {
            origin.source = "manifest";
            origin.location = Some(path.display().to_string());
            BootConfig::read_manifest(&path)
        } else {
            // An explicit root may name a staged package. Otherwise use only the executable/APK
            // report, never an unrelated build_info.json in the working directory.
            let local = self
                .project_root
                .as_ref()
                .map(|root| crate::packaged_config::PackagedConfig::read_directory(root));
            let package = match local.filter(|package| package.location().is_some()) {
                Some(package) => package,
                None => {
                    crate::packaged_config::ensure_loaded(app.world_mut());
                    app.world()
                        .resource::<crate::packaged_config::PackagedConfig>()
                        .clone()
                }
            };
            origin.location = package.location().map(str::to_string);
            match BootConfig::from_package(&package) {
                Ok(Some(config)) => {
                    origin.source = "packaged";
                    if let Some(directory) = package.directory() {
                        media_root = directory.join("assets");
                    }
                    Ok(config)
                }
                Ok(None) => {
                    origin.location = None;
                    Ok(BootConfig::default())
                }
                Err(error) => Err(error),
            }
        };
        let config = result.unwrap_or_else(|error| {
            origin.source = "invalid";
            tracing::warn!(target: "regia_framework", "boot movies ignored: {error}");
            BootConfig::default()
        });
        app.insert_resource(origin);
        if !config.movies.is_empty() && !app.world().contains_resource::<BootMovies>() {
            tracing::info!(target: "regia_framework", movies = config.movies.len(), "boot movies");
            app.insert_resource(config.sequence());
        }
        match app.world_mut().get_resource_mut::<MediaFiles>() {
            Some(mut files) if files.root().is_none() => files.set_root(media_root),
            Some(_) => {}
            None => {
                app.insert_resource(MediaFiles::with_root(media_root));
            }
        }
        if !app.world().contains_resource::<Messages<SkipVideo>>() {
            app.add_message::<SkipVideo>();
        }
        app.add_systems(Startup, add_skip_binding)
            .add_systems(Update, (video_context, map_skip_video).chain());
    }
}

/// `Startup`: the default `skip_video` bindings, unless the project binds the action.
fn add_skip_binding(config: Option<ResMut<InputConfig>>) {
    if let Some(mut config) = config {
        let next = std::mem::take(&mut *config).with_skip_video_default();
        *config = next;
    }
}

/// Enables the video context for player 0 while a boot movie or a skippable fullscreen video plays.
fn video_context(
    boot: Option<Res<BootMovies>>,
    players: Query<(&VideoPlayerState, &VideoFullscreen)>,
    contexts: Option<ResMut<InputContexts>>,
) {
    let Some(mut contexts) = contexts else {
        return;
    };
    let boot_playing = boot.is_some_and(|b| !b.is_done());
    let skippable = players
        .iter()
        .any(|(s, f)| f.skippable && s.core().holds_decoder());
    let want = boot_playing || skippable;
    let on = contexts.is_enabled(0, VIDEO_CONTEXT);
    if want && !on {
        contexts.push(0, VIDEO_CONTEXT);
    } else if !want && on {
        contexts.pop(0, VIDEO_CONTEXT);
    }
}

/// Turns a `skip_video` press into a `SkipVideo` message. `Update` (after the fixed loop) with
/// the frame-level edge: the step-level phase may already be consumed by a later step of a
/// multi-step frame, and `PreUpdate` runs before this frame's evaluation.
fn map_skip_video(actions: Option<Res<Actions>>, mut skips: MessageWriter<SkipVideo>) {
    if actions.is_some_and(|a| a.just_pressed_this_frame(SKIP_VIDEO_ACTION)) {
        skips.write(SkipVideo);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_table_parses_and_defaults() {
        let c = BootConfig::from_manifest(
            "[project]\nname = \"g\"\n[boot]\nmovies = [\"a.webm\", \"b.video.ron\"]\nskippable_after = 2.5\n",
        )
        .expect("parses");
        assert_eq!(c.movies, vec!["a.webm", "b.video.ron"]);
        let seq = c.sequence();
        assert_eq!(seq.movies.len(), 2);
        assert!((seq.skippable_after - 2.5).abs() < 1e-12);
        let none = BootConfig::from_manifest("[project]\nname = \"g\"\n").expect("parses");
        assert!(none.movies.is_empty());
        assert!(none.sequence().is_done());
        assert!(BootConfig::from_manifest("[boot]\nmovies = 3\n").is_err());
        let d = BootConfig::from_manifest("[boot]\nmovies = [\"a.webm\"]\n").expect("parses");
        assert!((d.sequence().skippable_after - DEFAULT_SKIPPABLE_AFTER).abs() < 1e-12);
    }

    #[test]
    fn packaged_boot_movies_without_a_loose_manifest() {
        let root = regia_test_utils::unique_temp_dir("packaged-boot-movies");
        std::fs::create_dir_all(&root).expect("directory");
        std::fs::write(
            root.join("build_info.json"),
            r#"{"boot":{"movies":["movies/intro.webm"],"skippable_after":2.5}}"#,
        )
        .expect("build info");
        let config = BootConfig::load(&root).expect("package config");
        assert_eq!(config.movies, ["movies/intro.webm"]);
        assert_eq!(config.skippable_after, Some(2.5));
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            regia_media::MediaPlugin,
            BootMoviesPlugin {
                project_root: Some(root.clone()),
                config: None,
            },
        ));
        assert_eq!(app.world().resource::<BootMovies>().movies, config.movies);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn packaged_boot_movies_play_and_loose_manifest_wins() {
        use regia_media::mux::PatternClip;
        let root = regia_test_utils::unique_temp_dir("packaged-boot-play");
        std::fs::create_dir_all(root.join("assets/movies")).expect("media dir");
        std::fs::write(
            root.join("assets/movies/intro.webm"),
            PatternClip::new(8, 8, 600, 60).to_webm(),
        )
        .expect("clip");
        std::fs::write(
            root.join("build_info.json"),
            r#"{"boot":{"movies":["movies/intro.webm"],"skippable_after":0.0}}"#,
        )
        .expect("metadata");
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            regia_media::MediaPlugin,
            BootMoviesPlugin {
                project_root: Some(root.clone()),
                config: None,
            },
        ));
        app.world_mut().resource_mut::<BootMovies>().clock = regia_media::ClockMode::Lockstep;
        app.update();
        app.update();
        assert!(app.world().resource::<BootMovies>().current().is_some());
        app.world_mut().write_message(SkipVideo);
        app.update();
        app.update();
        assert_eq!(
            app.world().resource::<BootMovies>().records()[0].outcome.name(),
            "skipped"
        );
        drop(app);
        std::fs::write(root.join("Regia.toml"), "[boot]\nmovies = []\n").expect("manifest");
        assert!(BootConfig::load(&root).expect("loose manifest").movies.is_empty());
        std::fs::write(root.join("Regia.toml"), "[boot]\nmovies = 5\n").expect("invalid manifest");
        assert!(
            BootConfig::load(&root).is_err(),
            "invalid loose config must not silently use package"
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn packaged_boot_metadata_failures_and_explicit_media_root() {
        let root = regia_test_utils::unique_temp_dir("packaged-boot-invalid");
        std::fs::create_dir_all(&root).expect("root");
        assert!(
            BootConfig::load(&root)
                .expect("missing metadata")
                .movies
                .is_empty()
        );
        for bytes in ["{broken", r#"{"boot":{"movies":7}}"#] {
            std::fs::write(root.join("build_info.json"), bytes).expect("metadata");
            assert!(BootConfig::load(&root).is_err());
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, regia_media::MediaPlugin));
            let extracted = root.join("extracted");
            app.insert_resource(MediaFiles::with_root(&extracted));
            app.add_plugins(BootMoviesPlugin {
                project_root: Some(root.clone()),
                config: None,
            });
            assert!(!app.world().contains_resource::<BootMovies>());
            assert_eq!(
                app.world().resource::<MediaFiles>().root(),
                Some(extracted.as_path())
            );
        }
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            regia_media::MediaPlugin,
            BootMoviesPlugin {
                project_root: Some(root.clone()),
                config: Some(BootConfig {
                    movies: vec!["code.webm".to_string()],
                    skippable_after: Some(3.0),
                }),
            },
        ));
        assert_eq!(app.world().resource::<BootMovies>().movies, ["code.webm"]);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn default_skip_binding_yields_to_a_project_binding() {
        let config = InputConfig::new().with_skip_video_default();
        let video = config.contexts.get(VIDEO_CONTEXT).expect("video context");
        assert_eq!(video.bindings.len(), 5);
        assert!(video.consume_lower);
        let own = InputConfig::new()
            .action(SKIP_VIDEO_ACTION, regia_input::ActionKind::Button)
            .binding(regia_input::Binding::new(
                SKIP_VIDEO_ACTION,
                regia_input::InputSource::key("KeyX"),
            ))
            .with_skip_video_default();
        assert!(!own.contexts.contains_key(VIDEO_CONTEXT));
    }

    /// End to end: the boot list from the manifest plays under the project root, and a `SkipVideo`
    /// skips it (`skippable_after = 0`).
    #[test]
    fn manifest_boot_movies_play_and_skip() {
        use regia_media::mux::PatternClip;
        let root = regia_test_utils::unique_temp_dir("boot_movies");
        std::fs::create_dir_all(root.join("movies")).expect("dir");
        std::fs::write(
            root.join("Regia.toml"),
            "[project]\nname = \"g\"\n[boot]\nmovies = [\"movies/a.webm\"]\nskippable_after = 0.0\n",
        )
        .expect("manifest");
        std::fs::write(
            root.join("movies/a.webm"),
            PatternClip::new(8, 8, 600, 60).to_webm(),
        )
        .expect("clip");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(regia_media::MediaPlugin)
            .add_plugins(BootMoviesPlugin {
                project_root: Some(root.clone()),
                config: None,
            });
        app.world_mut().resource_mut::<BootMovies>().clock = regia_media::ClockMode::Lockstep;
        app.update();
        app.update();
        let boot = app.world().resource::<BootMovies>();
        assert!(boot.current().is_some(), "the movie plays");
        app.world_mut().write_message(SkipVideo);
        app.update();
        app.update();
        let boot = app.world().resource::<BootMovies>();
        assert!(boot.is_done());
        assert_eq!(boot.records()[0].outcome.name(), "skipped");
        let _ = std::fs::remove_dir_all(root);
    }
}
