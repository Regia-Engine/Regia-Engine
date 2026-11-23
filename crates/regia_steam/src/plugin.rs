// SPDX-License-Identifier: MPL-2.0
//! [`SteamPlugin`] and [`SteamSettings`]: initialization, project-settings app id, and the
//! settings the overlay reaction reads.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_app::prelude::*;

use crate::achievements::{AchievementsConfig, Thresholds};
use crate::backend::{AppId, EventQueue, SteamBackend, SteamState, default_backend};
use crate::deck::{self, SteamDeck};
use crate::overlay::{OverlayActive, SuspendedContexts};
use crate::resource::Steam;

/// Filename Steam itself looks for in the working directory to learn the app id in dev builds;
/// `regia package --steam` writes it into the depot (specs/24 § 2 "Init").
pub const STEAM_APPID_FILE: &str = "steam_appid.txt";

/// Environment variable that overrides the app id (CI, dev shells).
pub const STEAM_APP_ID_ENV: &str = "STEAM_APP_ID";

/// The `[steam]` table of `Regia.toml`, as this crate reads it at startup (the packaging fields
/// `depots`/`branches` are parsed by `regia_cli`; only `app_id` matters at runtime).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
struct ManifestSteam {
    /// `[steam] app_id`.
    #[serde(default)]
    app_id: u32,
}

/// Project settings for Steam, insertable as a resource before the plugin runs (or set by the
/// game through [`SteamSettings`]'s fields). Fields the engine owns: what to do when the overlay
/// opens; where the achievements config lives.
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct SteamSettings {
    /// The app id, when the project settings are the source of truth. Lower priority than an
    /// explicit [`SteamPlugin::new`] id, higher than the filesystem fallbacks.
    pub app_id: Option<AppId>,
    /// Pause game time (`Time::paused`) while the Steam overlay is open. Off by default: the
    /// engine suspends gameplay *input* and the game decides about pausing (specs/24 § 2).
    pub pause_on_overlay: bool,
    /// The input contexts suspended while the overlay is open; `None` disables the engine's
    /// suspension (the game reacts to [`crate::OverlayOpened`] itself). Default: `["Gameplay"]`.
    pub suspend_contexts: Option<Vec<String>>,
    /// Explicit achievements config path; `None` uses the project convention
    /// `config/achievements.ron` under the working directory.
    pub achievements_path: Option<PathBuf>,
    /// Hand the mouse to the overlay while it is open (ADR-7349): from `OverlayOpened` to
    /// `OverlayClosed` the plugin sets `regia_platform::CursorRelease`, so every window shows a
    /// free cursor and a game that re-grabs the pointer (every frame, or on a focus change) cannot
    /// take it back from the overlay; the game's own grab is applied again after the close.
    /// Default: on.
    pub release_cursor_on_overlay: bool,
}

impl Default for SteamSettings {
    fn default() -> Self {
        Self {
            app_id: None,
            pause_on_overlay: false,
            suspend_contexts: Some(vec!["Gameplay".to_owned()]),
            achievements_path: None,
            release_cursor_on_overlay: true,
        }
    }
}

/// Steam plugin (specs/24 § 2 "Init"). Desktop, opt-in via the umbrella `steam` feature:
/// initializing talks to a running Steam client when the feature links the real backend, and
/// degrades to [`SteamState::Unavailable`] otherwise — the game always continues.
///
/// ```no_run
/// # fn main() {
/// let mut app = regia_app::App::new();
/// app.add_plugins(regia_steam::SteamPlugin::new(regia_steam::AppId(480)));
/// // `Res<regia_steam::Steam>` is now available to gameplay systems:
/// // steam.unlock("FIRST_BLOOD"), steam.stat_add("kills", 1), ...
/// # }
/// ```
///
/// The app id is resolved in this order:
///
/// 1. [`SteamPlugin::new`] (explicit),
/// 2. the [`SteamSettings`] resource (project settings the game inserted),
/// 3. `[steam] app_id` in the `Regia.toml` of the working directory (dev runs start there),
/// 4. the `STEAM_APP_ID` environment variable,
/// 5. `steam_appid.txt` next to the executable, then in the working directory (dev; the CLI
///    writes it when packaging),
/// 6. `None`: the Steamworks API resolves the app id itself (`steam_appid.txt` in the working
///    directory).
#[derive(Default)]
pub struct SteamPlugin {
    /// The app id, when the game knows it up front.
    pub app_id: Option<AppId>,
    /// An explicit backend (tests inject [`crate::MockSteam`]); `None` picks the default backend.
    pub(crate) backend: Option<Arc<dyn SteamBackend>>,
    /// An explicit achievements config; `None` loads [`SteamSettings::achievements_path`].
    pub(crate) achievements: Option<AchievementsConfig>,
}

impl SteamPlugin {
    /// A plugin with a known app id and default settings.
    pub fn new(app_id: AppId) -> Self {
        Self {
            app_id: Some(app_id),
            ..Self::default()
        }
    }

    /// A plugin that resolves the app id from project settings (see the type docs).
    pub fn from_project_settings() -> Self {
        Self::default()
    }

    /// Runs against an explicit backend instead of the real Steamworks client (tests, replays).
    pub fn with_backend(mut self, backend: Arc<dyn SteamBackend>) -> Self {
        self.backend = Some(backend);
        self
    }

    /// Uses this achievements config instead of loading `config/achievements.ron`.
    pub fn with_achievements(mut self, config: AchievementsConfig) -> Self {
        self.achievements = Some(config);
        self
    }

    /// The app id this plugin will initialize with, given the settings resources and the
    /// filesystem/env fallbacks. Public so doctor-style tooling can show it.
    pub fn resolve_app_id(&self, settings: &SteamSettings, cwd: &Path) -> Option<AppId> {
        self.resolve_app_id_with_env(
            settings,
            cwd,
            regia_core::brand::env_var(STEAM_APP_ID_ENV).ok().as_deref(),
        )
    }

    /// [`Self::resolve_app_id`] with the `STEAM_APP_ID` value passed in, so tests need not mutate
    /// the process environment.
    fn resolve_app_id_with_env(
        &self,
        settings: &SteamSettings,
        cwd: &Path,
        env_value: Option<&str>,
    ) -> Option<AppId> {
        self.app_id
            .or(settings.app_id)
            .or_else(|| app_id_from_manifest(cwd))
            .or_else(|| env_value?.parse::<u32>().ok().map(AppId))
            .or_else(|| app_id_from_file(&std::env::current_exe().ok()?.parent()?.join(STEAM_APPID_FILE)))
            .or_else(|| {
                let id = app_id_from_file(&cwd.join(STEAM_APPID_FILE))?;
                // The lowest source and the easiest to plant: a shipped game names its app id.
                tracing::warn!(target: "regia_steam", "app id {} comes from {STEAM_APPID_FILE} in the working directory", id.0);
                Some(id)
            })
    }
}

/// Reads one app id from a `steam_appid.txt` (a text file with just the number).
fn app_id_from_file(path: &Path) -> Option<AppId> {
    let text = crate::achievements::decode_text(&std::fs::read(path).ok()?)?;
    text.trim().parse::<u32>().ok().map(AppId)
}

/// Reads `[steam] app_id` from the project manifest in `dir`.
///
/// The manifest may be saved by a Windows editor as UTF-8 with BOM or UTF-16, so the bytes go
/// through [`crate::achievements::decode_text`] like `steam_appid.txt` and `achievements.ron`.
fn app_id_from_manifest(dir: &Path) -> Option<AppId> {
    let bytes = std::fs::read(dir.join("Regia.toml")).ok()?;
    let text = crate::achievements::decode_text(&bytes)?;
    let manifest: toml::Value = toml::from_str(&text).ok()?;
    let steam: Option<ManifestSteam> = manifest
        .get("steam")
        .cloned()
        .and_then(|value| value.try_into().ok());
    let Some(steam) = steam else {
        tracing::warn!(target: "regia_steam", "Regia.toml holds no usable [steam] app_id; falling back to STEAM_APP_ID / steam_appid.txt");
        return None;
    };
    if steam.app_id == 0 {
        tracing::warn!(target: "regia_steam", "Regia.toml holds [steam] app_id = 0; falling back to STEAM_APP_ID / steam_appid.txt");
        return None;
    }
    Some(AppId(steam.app_id))
}

impl Plugin for SteamPlugin {
    fn build(&self, app: &mut App) {
        let settings = app
            .world_mut()
            .get_resource_or_insert_with(SteamSettings::default)
            .clone();

        // Achievements config: explicit > configured path > default `config/achievements.ron`.
        // Missing or corrupt files degrade to an empty config with one warning (never fatal).
        let achievements = self.achievements.clone().unwrap_or_else(|| {
            let path = settings
                .achievements_path
                .clone()
                .unwrap_or_else(|| PathBuf::from("config").join("achievements.ron"));
            match AchievementsConfig::load_salvaged(&path) {
                Ok((config, problems)) => {
                    for problem in problems {
                        tracing::warn!(target: "regia_steam", "achievements config: {problem}");
                    }
                    config
                }
                Err(err) if is_not_found(&err) => AchievementsConfig::default(),
                Err(err) => {
                    tracing::warn!(target: "regia_steam", "achievements config: {err}");
                    AchievementsConfig::default()
                }
            }
        });
        let thresholds = Thresholds::from(&achievements);

        app.insert_resource(settings)
            .insert_resource(OverlayActive(false))
            .insert_resource(SuspendedContexts::default())
            .init_resource::<crate::SteamStatus>()
            .init_resource::<SteamDeck>()
            .init_resource::<crate::deck::DeckFrameCapSetting>()
            .init_resource::<crate::deck_prep::DeckPowerPolicy>()
            .init_resource::<crate::deck_prep::ThermalGovernor>()
            .init_resource::<crate::deck_prep::DeckSensorConfig>()
            .init_resource::<crate::deck_prep::ActiveGlyphs>()
            .init_resource::<crate::deck_prep::KeyboardState>()
            .add_message::<crate::deck_prep::TextFieldFocus>()
            .init_resource::<crate::deck_perf::DeckPerfSettings>()
            .init_resource::<crate::deck_perf::DeckPerfState>()
            .init_resource::<crate::deck_perf::DeckDisplay>()
            .add_message::<regia_platform::WindowResized>()
            .add_message::<regia_platform::GamepadConnected>()
            .add_message::<crate::overlay::OverlayOpened>()
            .add_message::<crate::overlay::OverlayClosed>()
            .add_message::<crate::JoinRequested>()
            .add_message::<crate::AchievementUnlocked>()
            .add_message::<crate::StatsStored>()
            .add_message::<crate::SteamDeckDetected>()
            .add_message::<crate::CloudConflict>()
            .init_resource::<crate::SteamInputSettings>()
            .init_resource::<crate::SteamInputStatus>()
            .init_resource::<crate::input::SteamInputRuntime>()
            .add_message::<regia_platform::GamepadButtonChanged>()
            .add_systems(PreUpdate, crate::pump::pump_steam)
            // The cursor hand-over to the overlay (ADR-7349).
            .init_resource::<regia_platform::CursorRelease>()
            .init_resource::<regia_platform::CursorCalls>()
            .add_systems(
                PreUpdate,
                crate::overlay::release_cursor_with_overlay.after(crate::pump::pump_steam),
            )
            .add_systems(
                PreUpdate,
                crate::input::steam_input_system
                    .after(crate::pump::pump_steam)
                    .before(regia_input::InputSet::Evaluation),
            )
            .add_systems(
                PreUpdate,
                crate::deck::apply_deck_frame_cap.run_if(crate::deck::deck_cap_dirty),
            )
            .add_systems(
                PreUpdate,
                (
                    crate::deck_prep::poll_deck_sensors.before(crate::deck::apply_deck_frame_cap),
                    crate::deck_prep::track_active_glyphs,
                    crate::deck_prep::drive_keyboard,
                ),
            )
            .add_systems(
                PreUpdate,
                (
                    crate::deck_perf::track_deck_display,
                    crate::deck_perf::update_deck_perf,
                )
                    .chain()
                    .after(crate::deck_prep::poll_deck_sensors)
                    .before(crate::deck::apply_deck_frame_cap),
            )
            .add_systems(
                PreUpdate,
                crate::deck_prep::follow_steam_input_glyphs
                    .after(crate::input::steam_input_system)
                    .after(crate::deck_prep::track_active_glyphs),
            )
            .add_systems(Lifecycle, crate::deck_prep::reset_on_resume)
            .add_systems(Lifecycle, crate::input::steam_input_on_lifecycle)
            .add_systems(Lifecycle, crate::pump::flush_stats_on_exit);

        crate::agent::install(app.world_mut());
        // REGIA_STEAM_OVERLAY_TRACE=<file>: the overlay trace for a bug report (ADR-7349).
        crate::overlay_trace::install_from_env(app);

        app.insert_resource(Steam {
            backend: self.backend.clone().unwrap_or_else(default_backend),
            state: SteamState::Unavailable,
            achievements: Arc::new(achievements),
            thresholds: Arc::new(thresholds),
            events: Arc::new(EventQueue::default()),
            overlay_open: false,
            deck: false,
            paused_by_overlay: false,
            announced: Arc::default(),
            known_unlocked: Arc::default(),
            timeline: Arc::default(),
        });
    }

    fn finish(&self, app: &mut App) {
        let settings = app.world().resource::<SteamSettings>().clone();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let app_id = self.resolve_app_id(&settings, &cwd);
        let backend = app.world().resource::<Steam>().backend.clone();

        // Valve's shipping step: a Steam-launched app must run through Steam. When it was not,
        // Steam relaunches us and this process must exit before initializing further.
        // The backend answers (ADR-5375): only the real one asks the SDK, so an injected mock
        // is initialized even in a `steamworks` build.
        if let Some(id) = app_id
            && backend.restart_app_if_necessary(id)
        {
            tracing::info!(
                target: "regia_steam",
                "Steam relaunched the app (id {id}); exiting this process"
            );
            {
                let mut steam = app.world_mut().resource_mut::<Steam>();
                steam.state = SteamState::Restarting;
            }
            app.world_mut().write_message(AppExit::Success);
            return;
        }

        // The one place availability is logged; afterwards every call degrades silently.
        let state = match backend.initialize(app_id) {
            Ok(SteamState::Ready) => {
                if let Some(id) = app_id {
                    tracing::info!(target: "regia_steam", "Steam ready (app {id})");
                } else {
                    tracing::info!(target: "regia_steam", "Steam ready");
                }
                SteamState::Ready
            }
            Ok(state) => state,
            Err(err) => {
                tracing::warn!(
                    target: "regia_steam",
                    "Steam unavailable ({err}); Steam features are off, the game continues"
                );
                SteamState::Unavailable
            }
        };

        publish_locale_hint(app, backend.as_ref(), state);

        let deck = deck::detect(
            backend.as_ref(),
            regia_core::brand::env_var("SteamDeck").ok().as_deref(),
        );
        {
            let mut steam = app.world_mut().resource_mut::<Steam>();
            steam.state = state;
            steam.deck = deck.detected;
        }
        let status = {
            let steam = app.world().resource::<Steam>();
            crate::SteamStatus::read(backend.as_ref(), steam.state, false)
        };
        app.insert_resource(status);
        app.insert_resource(deck);
        if deck.detected {
            // Before any pad reports in, a Deck shows its own controls' glyphs (ADR-1332).
            let mut glyphs = app.world_mut().resource_mut::<crate::deck_prep::ActiveGlyphs>();
            if glyphs.kind == crate::deck_prep::ControllerKind::Unknown {
                glyphs.kind = crate::deck_prep::ControllerKind::SteamDeck;
            }
            tracing::info!(
                target: "regia_steam",
                "Steam Deck detected: switching to the Deck render profile"
            );
            app.world_mut().write_message(crate::SteamDeckDetected);
        }
    }
}

/// Offers the player's per-game Steam language to the start-up locale choice (ADR-1618): a
/// connected Steam whose game language is in the table sets [`regia_platform::LocaleHints::steam`].
/// An unavailable Steam, an empty value or an unknown name sets nothing (the OS locale decides).
fn publish_locale_hint(app: &mut App, backend: &dyn SteamBackend, state: SteamState) {
    if state != SteamState::Ready {
        return;
    }
    let name = backend.game_language();
    match crate::steam_language_tag(&name) {
        Some(tag) => {
            app.world_mut()
                .get_resource_or_insert_with(regia_platform::LocaleHints::default)
                .steam = Some(tag.to_owned());
        }
        None if !name.trim().is_empty() => {
            tracing::warn!(target: "regia_steam", "unknown Steam game language {name:?}; ignored");
        }
        None => {}
    }
}

/// Whether the achievements error is just "file missing" (a fresh install boots without one).
fn is_not_found(err: &crate::achievements::AchievementsConfigError) -> bool {
    match err {
        crate::achievements::AchievementsConfigError::Read { source, .. } => source
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound),
        crate::achievements::AchievementsConfigError::Invalid(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("regia_steam_appid_{tag}_{}", std::process::id()))
    }

    #[test]
    fn resolves_explicit_app_id_first() {
        let plugin = SteamPlugin::new(AppId(480));
        let settings = SteamSettings::default();
        assert_eq!(
            plugin.resolve_app_id(&settings, Path::new("/nonexistent")),
            Some(AppId(480))
        );
    }

    #[test]
    fn resolves_from_settings_then_manifest_then_file() {
        let base = dir("order");
        std::fs::create_dir_all(&base).expect("temp dir");
        let plugin = SteamPlugin::from_project_settings();
        let mut settings = SteamSettings {
            app_id: Some(AppId(1000)),
            ..SteamSettings::default()
        };
        assert_eq!(plugin.resolve_app_id(&settings, &base), Some(AppId(1000)));
        settings.app_id = None;

        std::fs::write(base.join("Regia.toml"), "[steam]\napp_id = 777\n").expect("manifest");
        assert_eq!(plugin.resolve_app_id(&settings, &base), Some(AppId(777)));
        // A manifest without [steam] or with app_id = 0 falls through.
        std::fs::write(base.join("Regia.toml"), "[steam]\napp_id = 0\n").expect("manifest");
        assert_eq!(plugin.resolve_app_id_with_env(&settings, &base, None), None);
        std::fs::write(base.join("Regia.toml"), "[project]\nname = \"x\"\n").expect("manifest");
        assert_eq!(plugin.resolve_app_id_with_env(&settings, &base, None), None);

        std::fs::write(base.join(STEAM_APPID_FILE), " 480 \n").expect("appid file");
        assert_eq!(plugin.resolve_app_id(&settings, &base), Some(AppId(480)));
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn no_app_id_anywhere_is_none() {
        let plugin = SteamPlugin::from_project_settings();
        let settings = SteamSettings {
            app_id: None,
            ..SteamSettings::default()
        };
        // The env value is injected: tests run on parallel threads, so none mutates the process env.
        assert_eq!(
            plugin.resolve_app_id_with_env(&settings, Path::new("/nonexistent"), None),
            None
        );
    }

    #[test]
    fn app_id_file_may_be_utf16_or_have_a_bom() {
        let base = regia_test_utils::unique_temp_dir("steam-appid-enc");
        std::fs::create_dir_all(&base).expect("temp dir");
        let file = base.join(STEAM_APPID_FILE);
        std::fs::write(&file, b"\xEF\xBB\xBF480\r\n").expect("bom");
        assert_eq!(app_id_from_file(&file), Some(AppId(480)));
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("1234\r\n".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        std::fs::write(&file, utf16).expect("utf16");
        assert_eq!(app_id_from_file(&file), Some(AppId(1234)));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn manifest_with_a_utf8_bom_still_yields_the_app_id() {
        // A Windows editor saves "UTF-8 with BOM" by an explicit option; the manifest source
        // must decode like steam_appid.txt and achievements.ron (audit G, finding 4).
        let base = regia_test_utils::unique_temp_dir("steam-manifest-bom");
        std::fs::write(base.join("Regia.toml"), b"\xEF\xBB\xBF[steam]\napp_id = 777\n")
            .expect("bom manifest");
        assert_eq!(app_id_from_manifest(&base), Some(AppId(777)));
        // UTF-16 LE (Notepad's other encoding) decodes too.
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain(
                "[steam]\napp_id = 555\n"
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes),
            )
            .collect();
        std::fs::write(base.join("Regia.toml"), utf16).expect("utf16 manifest");
        assert_eq!(app_id_from_manifest(&base), Some(AppId(555)));
        let _ = std::fs::remove_dir_all(&base);
    }

    fn hint_after(game_language: &str, state: SteamState) -> Option<String> {
        let mock = crate::MockSteam::shared();
        mock.set_game_language(game_language);
        let mut app = App::new();
        publish_locale_hint(&mut app, mock.as_ref(), state);
        app.world()
            .get_resource::<regia_platform::LocaleHints>()
            .and_then(|hints| hints.steam.clone())
    }

    #[test]
    fn a_connected_steam_offers_its_game_language_as_a_locale_hint() {
        assert_eq!(hint_after("japanese", SteamState::Ready).as_deref(), Some("ja"));
        assert_eq!(
            hint_after("tchinese", SteamState::Ready).as_deref(),
            Some("zh-Hant")
        );
    }

    #[test]
    fn no_hint_when_steam_is_unavailable_or_the_language_is_unknown() {
        assert_eq!(hint_after("japanese", SteamState::Unavailable), None);
        assert_eq!(hint_after("klingon", SteamState::Ready), None);
        assert_eq!(hint_after("", SteamState::Ready), None);
    }
}
