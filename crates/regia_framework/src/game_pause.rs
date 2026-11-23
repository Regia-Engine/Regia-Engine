// SPDX-License-Identifier: MPL-2.0
//! The game pause key and the built-in "Paused" screen (ADR-10104, book `user-guide/pausing.md`,
//! agent guide `agents/game-pause.md`).
//!
//! A game turns the pause on in `Regia.toml` (off by default; the keys are additive, so older
//! manifests read unchanged and there is no format bump, `docs/formats/regia-toml-game.md`):
//!
//! ```toml
//! [game]
//! pause = true               # the pause key works in this game (default false)
//! pause_key = "Pause"        # the keyboard key of the TogglePause action ("none": no key)
//! pause_gamepad = "Start"    # a gamepad button for it (default: none, off)
//! pause_screen = "default"   # "default" | "none" | "<ui asset path>" (ui/pause.ui.ron)
//! ```
//!
//! [`GamePausePlugin`] reads the table once. When the pause is on it adds the rebindable
//! [`TOGGLE_PAUSE_ACTION`] in the consuming [`PAUSE_CONTEXT`] (unless the project binds the action
//! itself, so a project binding and a player's rebinding win), enables that context for player 0,
//! and each frame turns a press into a pause or a resume. An app without `regia_input`'s action layer
//! (no `InputConfig`) gets the configured key and gamepad button read from the raw events instead.
//!
//! **What a pause does.** It sets [`Time::paused`]: the gameplay delta is zero, the fixed loop runs no
//! step, so physics, animation, AI and everything else that reads gameplay time stands still (UI and
//! audio read `RealTime`). Every player's input contexts are reduced to [`PAUSE_CONTEXT`] plus the
//! game's [`GamePauseConfig::keep_contexts`], so gameplay actions read idle; UI navigation (keys,
//! pointer, the gamepad d-pad mapped here) and the screen's buttons keep working. The OS cursor is
//! released ([`regia_platform::CursorRelease`]: free and visible on desktop; nothing to do on touch
//! devices). Resuming puts the contexts back, re-grabs the cursor the way the game's window asks
//! for it, and clears [`Time::paused`] unless something else had paused time before (the agent's
//! `play.pause`, the focus-loss pause). [`GamePaused`] and [`GameResumed`] are sent on each change;
//! the run conditions [`game_paused`] and [`game_running`] gate systems on it.
//!
//! **Networked sessions.** A peer cannot freeze the other players' worlds, so a pause in a networked
//! session ([`NetworkedSession`], or a `regia_net` `NetMode` other than `Offline` with the `net`
//! feature) is refused with [`PauseRefusal::NetworkedSession`] and a [`PauseRefused`] message. The
//! host-authoritative pause is ADR-10104's later step.
//!
//! **The screen.** [`PauseScreen::Default`] is a built-in overlay: a centered "Paused" title, a hint
//! naming the bound key ("Press Pause to resume"), and Resume and Quit-to-menu buttons; strings come
//! from the game's [`Localization`] when it defines a `pause-*` key, otherwise from the bundled
//! `locales/<lang>/game_pause.ftl` (en, de, fr, es). It is a dialog for screen readers, keyboard and
//! gamepad navigable, on a UI-only input layer. [`PauseScreen::None`] shows nothing (the game draws
//! its own from [`GamePaused`]), [`PauseScreen::Asset`] spawns a `.ui.ron` template, and
//! [`PauseScreen::Custom`] calls the game's builder. Buttons with the UI command
//! [`RESUME_COMMAND`] or [`QUIT_TO_MENU_COMMAND`] (the built-in ones, or any node of a replacement
//! screen) send [`PauseScreenEvent`]; both end the pause, and Quit-to-menu is the game's to handle
//! (load its menu level).
//!
//! ```no_run
//! use regia_app::prelude::*;
//! use regia_framework::game_pause::{GamePause, GamePauseConfig, GamePausePlugin};
//!
//! let mut app = App::new();
//! app.add_plugins(GamePausePlugin {
//!     config: Some(GamePauseConfig::enabled()),
//!     ..Default::default()
//! });
//! // From code, e.g. an in-game menu button:
//! app.world_mut().resource_mut::<GamePause>().toggle();
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regia_app::prelude::*;
use regia_ecs::ecs::message::MessageCursor;
use regia_input::{
    Actions, Binding, Gamepads, InputConfig, InputContext, InputContexts, InputDefaults, InputOverrides,
    InputSource,
};
use regia_platform::{ButtonState, CursorRelease, GamepadButton, KeyboardInput};
use regia_ui::a11y::{A11yRole, Accessible};
use regia_ui::focus::{DefaultFocus, FocusScope, UiNavActions};
use regia_ui::node::{Background, BackgroundKind, NodeStyle, UiNode, UiRoot};
use regia_ui::style::{Align, Dimension, Edges, FlexDirection, JustifyContent, Style};
use regia_ui::text::{Text, TextAlign, TextSpan, TextStyle, WrapMode};
use regia_ui::viewmodel::{UiCommand, UiCommandName};
use regia_ui::widgets::spawn_button;
use regia_ui::{InputLayer, LayerInputMode, Localization, ScaleMode, UiColor};
use serde::{Deserialize, Serialize};

/// The input action that pauses and resumes the game.
pub const TOGGLE_PAUSE_ACTION: &str = "TogglePause";

/// The context of the [`TOGGLE_PAUSE_ACTION`] bindings: consuming (priority 1000), enabled for player
/// 0 while the pause is on, and left enabled during a pause (with UI navigation's `Menu` context and
/// [`GamePauseConfig::keep_contexts`]).
pub const PAUSE_CONTEXT: &str = "Pause";

/// The default keyboard key of [`TOGGLE_PAUSE_ACTION`]: Pause/Break.
pub const DEFAULT_PAUSE_KEY: &str = "Pause";

/// The UI command of a "Resume" button ([`PauseScreenEvent::Resume`]).
pub const RESUME_COMMAND: &str = "pause.resume";

/// The UI command of a "Quit to menu" button ([`PauseScreenEvent::QuitToMenu`]).
pub const QUIT_TO_MENU_COMMAND: &str = "pause.quit_to_menu";

/// Draw order of the built-in screen's root (above game UI, below the calibration screen).
const SCREEN_SORT_ORDER: i32 = 900;

/// Bundled strings: `(locale, ftl)`.
const BUNDLED_FTL: &[(&str, &str)] = &[
    ("en", include_str!("../locales/en/game_pause.ftl")),
    ("de", include_str!("../locales/de/game_pause.ftl")),
    ("fr", include_str!("../locales/fr/game_pause.ftl")),
    ("es", include_str!("../locales/es/game_pause.ftl")),
];

/// The gamepad buttons by the names `pause_gamepad` accepts.
const GAMEPAD_BUTTONS: &[(&str, GamepadButton)] = &[
    ("South", GamepadButton::South),
    ("East", GamepadButton::East),
    ("West", GamepadButton::West),
    ("North", GamepadButton::North),
    ("LeftShoulder", GamepadButton::LeftShoulder),
    ("RightShoulder", GamepadButton::RightShoulder),
    ("LeftTrigger", GamepadButton::LeftTrigger),
    ("RightTrigger", GamepadButton::RightTrigger),
    ("LeftStick", GamepadButton::LeftStick),
    ("RightStick", GamepadButton::RightStick),
    ("DPadUp", GamepadButton::DPadUp),
    ("DPadDown", GamepadButton::DPadDown),
    ("DPadLeft", GamepadButton::DPadLeft),
    ("DPadRight", GamepadButton::DPadRight),
    ("Start", GamepadButton::Start),
    ("Select", GamepadButton::Select),
    ("Mode", GamepadButton::Mode),
];

/// The name `pause_gamepad` uses for `button`.
#[must_use]
pub fn gamepad_button_name(button: GamepadButton) -> &'static str {
    GAMEPAD_BUTTONS
        .iter()
        .find(|(_, b)| *b == button)
        .map_or("Start", |(name, _)| name)
}

/// Parses a `pause_gamepad` name (`"Start"`, `"Select"`, `"South"`, ...).
#[must_use]
pub fn parse_gamepad_button(name: &str) -> Option<GamepadButton> {
    GAMEPAD_BUTTONS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, button)| *button)
}

/// A game's own screen builder for [`PauseScreen::Custom`]: spawns the screen and returns its root
/// entity, which the pause despawns (recursively) on resume.
#[derive(Clone)]
pub struct PauseScreenBuilder(pub Arc<dyn Fn(&mut World) -> Entity + Send + Sync>);

impl std::fmt::Debug for PauseScreenBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PauseScreenBuilder(..)")
    }
}

/// What shows while the game is paused.
#[derive(Debug, Clone, Default)]
pub enum PauseScreen {
    /// The built-in overlay (see the module docs).
    #[default]
    Default,
    /// Nothing: the game draws its own screen from [`GamePaused`].
    None,
    /// A `.ui.ron` template: the project path as written in `Regia.toml` (`ui/pause.ui.ron`,
    /// `assets://ui/pause.ui.ron` or the template name `pause`). A template that is not loaded logs
    /// `pause_screen_missing` and the built-in screen shows instead.
    Asset(String),
    /// A screen the game builds in code.
    Custom(PauseScreenBuilder),
}

impl PartialEq for PauseScreen {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Default, Self::Default) | (Self::None, Self::None) => true,
            (Self::Asset(a), Self::Asset(b)) => a == b,
            (Self::Custom(a), Self::Custom(b)) => Arc::ptr_eq(&a.0, &b.0),
            _ => false,
        }
    }
}

impl PauseScreen {
    /// A screen built by `build` (see [`PauseScreen::Custom`]).
    pub fn custom(build: impl Fn(&mut World) -> Entity + Send + Sync + 'static) -> Self {
        Self::Custom(PauseScreenBuilder(Arc::new(build)))
    }

    /// Parses a `pause_screen` value: `default`, `none` or a UI asset path. An empty value is the
    /// default screen.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value.trim() {
            "" | "default" => Self::Default,
            "none" => Self::None,
            path => Self::Asset(path.to_owned()),
        }
    }

    /// The `pause_screen` value of this choice (`custom` for a code builder, which has no file form).
    #[must_use]
    pub fn setting(&self) -> String {
        match self {
            Self::Default => "default".to_owned(),
            Self::None => "none".to_owned(),
            Self::Asset(path) => path.clone(),
            Self::Custom(_) => "custom".to_owned(),
        }
    }

    /// The kind name the agent reports: `default`, `none`, `asset` or `custom`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::None => "none",
            Self::Asset(_) => "asset",
            Self::Custom(_) => "custom",
        }
    }
}

/// The UI template name of a pause-screen asset path: `assets://ui/pause.ui.ron`, `ui/pause.ui.ron`
/// and `pause` all name the template `pause` (templates load from `assets://ui`).
#[must_use]
pub fn ui_template_name(path: &str) -> String {
    let path = path.trim();
    let path = path.strip_prefix("assets://").unwrap_or(path);
    let path = path.strip_prefix("ui/").unwrap_or(path);
    path.strip_suffix(".ui.ron").unwrap_or(path).to_owned()
}

/// The pause settings of a game (`Regia.toml` `[game]`, or code).
#[derive(Debug, Clone, PartialEq)]
pub struct GamePauseConfig {
    /// The pause works in this game (`pause`). Off by default.
    pub enabled: bool,
    /// The keyboard key of [`TOGGLE_PAUSE_ACTION`] (`pause_key`, a physical key name); `None` binds
    /// no key.
    pub key: Option<String>,
    /// The gamepad button of [`TOGGLE_PAUSE_ACTION`] (`pause_gamepad`); `None` (the default) binds no
    /// button.
    pub gamepad: Option<GamepadButton>,
    /// What shows while paused (`pause_screen`).
    pub screen: PauseScreen,
    /// Input contexts that stay enabled during a pause besides [`PAUSE_CONTEXT`] (a game's own menu
    /// context). Code only.
    pub keep_contexts: Vec<String>,
}

impl Default for GamePauseConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            key: Some(DEFAULT_PAUSE_KEY.to_owned()),
            gamepad: None,
            screen: PauseScreen::Default,
            keep_contexts: Vec::new(),
        }
    }
}

/// The `[game]` table of `Regia.toml` as written (the pause keys; other keys are ignored).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
struct GameTable {
    #[serde(skip_serializing_if = "Option::is_none")]
    pause: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pause_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pause_gamepad: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pause_screen: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Manifest {
    #[serde(default)]
    game: GameTable,
}

#[derive(Serialize)]
struct ManifestOut<'a> {
    game: &'a GameTable,
}

/// A `pause_key` / `pause_gamepad` value that means "no binding".
fn is_none_value(value: &str) -> bool {
    matches!(value.trim(), "" | "none")
}

impl GamePauseConfig {
    /// The pause turned on with the default key, no gamepad button and the built-in screen.
    #[must_use]
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    /// Parses the `[game]` table out of a whole `Regia.toml`; an absent table is the default (off).
    /// A key of the wrong type, or an unknown `pause_gamepad` name, is an error naming the table.
    pub fn from_manifest(text: &str) -> Result<Self, String> {
        let table = toml::from_str::<Manifest>(text)
            .map(|m| m.game)
            .map_err(|e| format!("Regia.toml [game]: {e}"))?;
        Self::from_table(table)
    }

    /// Reads the `game` object `regia package` copies into `build_info.json` (a packaged game has no
    /// loose `Regia.toml`); `None` when the package has none.
    pub fn from_package(package: &crate::packaged_config::PackagedConfig) -> Result<Option<Self>, String> {
        let Some(value) = package.game().map_err(str::to_string)? else {
            return Ok(None);
        };
        let table = serde_json::from_value::<GameTable>(value.clone())
            .map_err(|e| format!("Regia.toml [game]: build_info.json game: {e}"))?;
        Self::from_table(table).map(Some)
    }

    fn from_table(table: GameTable) -> Result<Self, String> {
        let gamepad = match table.pause_gamepad.as_deref() {
            None => None,
            Some(name) if is_none_value(name) => None,
            Some(name) => Some(parse_gamepad_button(name.trim()).ok_or_else(|| {
                format!(
                    "Regia.toml [game]: unknown pause_gamepad {name:?} (one of {})",
                    GAMEPAD_BUTTONS
                        .iter()
                        .map(|(n, _)| *n)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?),
        };
        let key = match table.pause_key.as_deref() {
            None => Some(DEFAULT_PAUSE_KEY.to_owned()),
            Some(key) if is_none_value(key) => None,
            Some(key) => Some(key.trim().to_owned()),
        };
        Ok(Self {
            enabled: table.pause.unwrap_or(false),
            key,
            gamepad,
            screen: table
                .pause_screen
                .as_deref()
                .map(PauseScreen::parse)
                .unwrap_or_default(),
            keep_contexts: Vec::new(),
        })
    }

    /// Reads the project's manifest under `root`; a missing manifest is the default (off).
    pub fn load(root: &Path) -> Result<Self, String> {
        let Some(path) = regia_core::brand::project_manifest(root) else {
            return Ok(Self::default());
        };
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::from_manifest(&text)
    }

    /// The `[game]` table that reads back as this config (every key written). A
    /// [`PauseScreen::Custom`] screen has no file form and is written as `default`.
    #[must_use]
    pub fn to_toml(&self) -> String {
        let screen = match &self.screen {
            PauseScreen::Custom(_) => "default".to_owned(),
            other => other.setting(),
        };
        let table = GameTable {
            pause: Some(self.enabled),
            pause_key: Some(self.key.clone().unwrap_or_else(|| "none".to_owned())),
            pause_gamepad: Some(self.gamepad.map_or("none", gamepad_button_name).to_owned()),
            pause_screen: Some(screen),
        };
        toml::to_string(&ManifestOut { game: &table }).unwrap_or_default()
    }
}

/// Adds the [`TOGGLE_PAUSE_ACTION`] bindings of `config` to `input` in [`PAUSE_CONTEXT`], unless the
/// input config binds the action somewhere already (a project binding wins). Declares the action.
/// Returns whether bindings were added.
pub fn add_pause_bindings(input: &mut InputConfig, config: &GamePauseConfig) -> bool {
    input
        .actions
        .entry(TOGGLE_PAUSE_ACTION.to_owned())
        .or_insert(regia_input::ActionKind::Button);
    let bound = input
        .contexts
        .values()
        .any(|c| c.bindings.iter().any(|b| b.action == TOGGLE_PAUSE_ACTION));
    if bound {
        return false;
    }
    let mut context = input
        .contexts
        .remove(PAUSE_CONTEXT)
        .unwrap_or_else(|| InputContext::consuming(1000));
    if let Some(key) = &config.key {
        context
            .bindings
            .push(Binding::new(TOGGLE_PAUSE_ACTION, InputSource::key(key.as_str())));
    }
    if let Some(button) = config.gamepad {
        context.bindings.push(Binding::new(
            TOGGLE_PAUSE_ACTION,
            InputSource::gamepad_button(button),
        ));
    }
    input.contexts.insert(PAUSE_CONTEXT.to_owned(), context);
    true
}

/// Who asked for a pause change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseSource {
    /// The [`TOGGLE_PAUSE_ACTION`] (the pause key, a gamepad button, or a rebinding of them).
    Input,
    /// A button of the pause screen.
    Screen,
    /// Game code ([`GamePause::pause`], [`GamePause::resume`], [`GamePause::toggle`]).
    Code,
    /// The agent protocol (`game.pause` / `game.resume`).
    Agent,
}

impl PauseSource {
    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Screen => "screen",
            Self::Code => "code",
            Self::Agent => "agent",
        }
    }
}

/// Why a pause was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseRefusal {
    /// The game did not turn the pause on (`[game] pause = false`).
    Disabled,
    /// A networked session runs: a peer cannot freeze the other players' worlds.
    NetworkedSession,
}

impl PauseRefusal {
    /// The reason code (`game_pause_off`, `networked_session`).
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Disabled => "game_pause_off",
            Self::NetworkedSession => "networked_session",
        }
    }

    /// A sentence for logs and the agent.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::Disabled => "the game has not turned the pause on ([game] pause = true in Regia.toml)",
            Self::NetworkedSession => {
                "pause is refused in a networked session: a peer cannot freeze the other players' worlds"
            }
        }
    }
}

/// Sent when the game pauses.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct GamePaused {
    /// Who paused it.
    pub source: PauseSource,
}

/// Sent when the game resumes.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct GameResumed {
    /// Who resumed it.
    pub source: PauseSource,
}

/// Sent when a pause was refused.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PauseRefused {
    /// Who asked.
    pub source: PauseSource,
    /// Why it was refused.
    pub reason: PauseRefusal,
}

/// A pause-screen button was pressed (the built-in screen's, or any node with the UI command
/// [`RESUME_COMMAND`] / [`QUIT_TO_MENU_COMMAND`]). Both end the pause first.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseScreenEvent {
    /// Resume.
    Resume,
    /// Quit to the menu: the game loads its menu level.
    QuitToMenu,
}

/// Marks this process as part of a networked session for the pause (a game whose transport is not
/// `regia_net` sets it; with `regia_net` the `NetMode` is read as well).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetworkedSession(pub bool);

/// The `Update` set the pause key, code requests and screen commands are applied in: order a system
/// `.after(GamePauseSet)` to see this frame's pause state.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GamePauseSet;

/// Marks the root of the screen the pause spawned.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PauseScreenRoot;

/// The text of the built-in screen's key hint (tests and the agent read it).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PauseScreenHint;

/// A queued pause change and who asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    Pause(PauseSource),
    Resume(PauseSource),
    Toggle(PauseSource),
}

/// The screen showing: its root, its kind and the strings it was built with.
#[derive(Debug, Clone)]
struct Shown {
    root: Entity,
    kind: &'static str,
    /// Locale generation and hint text the built-in screen was built with (rebuilt on change).
    built_for: Option<(u64, String)>,
}

/// The game pause: settings, state and requests (see the module docs).
#[derive(Resource)]
pub struct GamePause {
    config: GamePauseConfig,
    paused: bool,
    source: Option<PauseSource>,
    since_frame: Option<u64>,
    /// `Time::paused` was already set when the pause began: resume leaves it set.
    time_was_paused: bool,
    /// The pause set `CursorRelease` and clears it on resume.
    cursor_ours: bool,
    saved_contexts: BTreeMap<u8, Vec<String>>,
    shown: Option<Shown>,
    screen_error: Option<String>,
    requests: Vec<Request>,
    last_refusal: Option<PauseRefusal>,
    pauses: u64,
    resumes: u64,
    refusals: u64,
    strings: Option<Localization>,
    command_cursor: MessageCursor<UiCommand>,
    /// Raw key events, read only in an app without the action layer (no `InputConfig`).
    key_cursor: MessageCursor<KeyboardInput>,
    /// The default bindings were added to the input config (once, when the pause is first on).
    bindings_added: bool,
    /// [`TOGGLE_PAUSE_ACTION`] had a value last frame (the level edge, see `toggle_pressed`).
    toggle_held: bool,
}

impl GamePause {
    /// A pause with `config`, not paused.
    #[must_use]
    pub fn new(config: GamePauseConfig) -> Self {
        Self {
            config,
            paused: false,
            source: None,
            since_frame: None,
            time_was_paused: false,
            cursor_ours: false,
            saved_contexts: BTreeMap::new(),
            shown: None,
            screen_error: None,
            requests: Vec::new(),
            last_refusal: None,
            pauses: 0,
            resumes: 0,
            refusals: 0,
            strings: None,
            command_cursor: MessageCursor::default(),
            key_cursor: MessageCursor::default(),
            bindings_added: false,
            toggle_held: false,
        }
    }

    /// The settings.
    #[must_use]
    pub fn config(&self) -> &GamePauseConfig {
        &self.config
    }

    /// Whether the game turned the pause on.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Turns the pause on or off for this game (a settings menu); the key works from the next update.
    /// Turning it off while paused resumes on the next update.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.config.enabled = enabled;
        if !enabled && self.paused {
            self.requests.push(Request::Resume(PauseSource::Code));
        }
    }

    /// Replaces what shows while paused; a screen already showing is replaced on the next update.
    pub fn set_screen(&mut self, screen: PauseScreen) {
        self.config.screen = screen;
    }

    /// Whether the game is paused.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Who paused the game (while paused).
    #[must_use]
    pub fn source(&self) -> Option<PauseSource> {
        self.source
    }

    /// The frame the pause began on (while paused).
    #[must_use]
    pub fn since_frame(&self) -> Option<u64> {
        self.since_frame
    }

    /// The kind of screen showing (`default`, `asset`, `custom`), `None` when nothing shows.
    #[must_use]
    pub fn screen_shown(&self) -> Option<&'static str> {
        self.shown.as_ref().map(|s| s.kind)
    }

    /// The root entity of the screen showing.
    #[must_use]
    pub fn screen_root(&self) -> Option<Entity> {
        self.shown.as_ref().map(|s| s.root)
    }

    /// Why the configured screen could not be shown the last time (the built-in one showed instead).
    #[must_use]
    pub fn screen_error(&self) -> Option<&str> {
        self.screen_error.as_deref()
    }

    /// The last refusal.
    #[must_use]
    pub fn last_refusal(&self) -> Option<PauseRefusal> {
        self.last_refusal
    }

    /// `(pauses, resumes, refusals)` since start.
    #[must_use]
    pub fn counts(&self) -> (u64, u64, u64) {
        (self.pauses, self.resumes, self.refusals)
    }

    /// Pauses on the next update (refused like the key: off, or networked).
    pub fn pause(&mut self) {
        self.requests.push(Request::Pause(PauseSource::Code));
    }

    /// Resumes on the next update.
    pub fn resume(&mut self) {
        self.requests.push(Request::Resume(PauseSource::Code));
    }

    /// Pauses or resumes on the next update.
    pub fn toggle(&mut self) {
        self.requests.push(Request::Toggle(PauseSource::Code));
    }
}

impl Default for GamePause {
    fn default() -> Self {
        Self::new(GamePauseConfig::default())
    }
}

/// Run condition: the game is paused.
pub fn game_paused(pause: Option<Res<GamePause>>) -> bool {
    pause.is_some_and(|p| p.is_paused())
}

/// Run condition: the game is not paused (also true without the plugin).
pub fn game_running(pause: Option<Res<GamePause>>) -> bool {
    !game_paused(pause)
}

/// Whether this process is in a networked session (see [`NetworkedSession`]).
#[must_use]
pub fn networked_session(world: &World) -> bool {
    if world.get_resource::<NetworkedSession>().is_some_and(|n| n.0) {
        return true;
    }
    #[cfg(feature = "net")]
    if world
        .get_resource::<regia_net::NetMode>()
        .is_some_and(|mode| *mode != regia_net::NetMode::Offline)
    {
        return true;
    }
    false
}

/// Whether a pause would be refused now, and why.
#[must_use]
pub fn pause_refusal(world: &World) -> Option<PauseRefusal> {
    let enabled = world
        .get_resource::<GamePause>()
        .is_some_and(GamePause::is_enabled);
    if !enabled {
        return Some(PauseRefusal::Disabled);
    }
    if networked_session(world) {
        return Some(PauseRefusal::NetworkedSession);
    }
    None
}

/// Pauses now: freezes gameplay time, reduces the input contexts, releases the cursor, shows the
/// screen and sends [`GamePaused`]. `Ok(false)` when already paused; a refusal sends
/// [`PauseRefused`] and logs it.
pub fn pause_now(world: &mut World, source: PauseSource) -> Result<bool, PauseRefusal> {
    if !world.contains_resource::<GamePause>() {
        return Err(PauseRefusal::Disabled);
    }
    if world.resource::<GamePause>().paused {
        return Ok(false);
    }
    if let Some(reason) = pause_refusal(world) {
        let mut pause = world.resource_mut::<GamePause>();
        pause.last_refusal = Some(reason);
        pause.refusals += 1;
        tracing::info!(target: "regia_framework", reason = reason.code(), source = source.name(), "pause refused");
        world.write_message(PauseRefused { source, reason });
        return Err(reason);
    }
    let frame = world.get_resource::<Time>().map(Time::frame_count);
    let time_was_paused = match world.get_resource_mut::<Time>() {
        Some(mut time) => {
            let was = time.paused;
            time.paused = true;
            was
        }
        None => false,
    };
    let saved = reduce_contexts(world);
    let cursor_ours = match world.get_resource_mut::<CursorRelease>() {
        Some(mut release) if !release.0 => {
            release.0 = true;
            true
        }
        _ => false,
    };
    {
        let mut pause = world.resource_mut::<GamePause>();
        pause.paused = true;
        pause.source = Some(source);
        pause.since_frame = frame;
        pause.time_was_paused = time_was_paused;
        pause.cursor_ours = cursor_ours;
        pause.saved_contexts = saved;
        pause.pauses += 1;
    }
    tracing::info!(target: "regia_framework", source = source.name(), "game paused");
    show_screen(world);
    world.write_message(GamePaused { source });
    Ok(true)
}

/// Resumes now: the reverse of [`pause_now`], then [`GameResumed`]. Returns `false` when the game was
/// not paused.
pub fn resume_now(world: &mut World, source: PauseSource) -> bool {
    let Some(pause) = world.get_resource::<GamePause>() else {
        return false;
    };
    if !pause.paused {
        return false;
    }
    hide_screen(world);
    let (time_was_paused, cursor_ours, saved) = {
        let mut pause = world.resource_mut::<GamePause>();
        pause.paused = false;
        pause.source = None;
        pause.since_frame = None;
        pause.resumes += 1;
        (
            pause.time_was_paused,
            std::mem::take(&mut pause.cursor_ours),
            std::mem::take(&mut pause.saved_contexts),
        )
    };
    if !time_was_paused && let Some(mut time) = world.get_resource_mut::<Time>() {
        time.paused = false;
    }
    restore_contexts(world, saved);
    if cursor_ours && let Some(mut release) = world.get_resource_mut::<CursorRelease>() {
        release.0 = false;
    }
    tracing::info!(target: "regia_framework", source = source.name(), "game resumed");
    world.write_message(GameResumed { source });
    true
}

/// The contexts that stay enabled during a pause: the pause context, UI navigation's `Menu` context
/// (its actions drive focus navigation) and the game's `keep_contexts`.
fn kept(world: &World, context: &str) -> bool {
    context == PAUSE_CONTEXT
        || context == regia_input::MENU_CONTEXT
        || world
            .get_resource::<GamePause>()
            .is_some_and(|p| p.config.keep_contexts.iter().any(|k| k == context))
}

/// Reduces every player's contexts to the kept ones; returns the stacks it replaced.
fn reduce_contexts(world: &mut World) -> BTreeMap<u8, Vec<String>> {
    let mut saved = BTreeMap::new();
    let Some(contexts) = world.get_resource::<InputContexts>() else {
        return saved;
    };
    let mut next = Vec::new();
    for player in 0..=u8::MAX {
        let stack = contexts.enabled(player);
        if stack.is_empty() {
            continue;
        }
        let keep: Vec<String> = stack.iter().filter(|c| kept(world, c)).cloned().collect();
        saved.insert(player, stack.to_vec());
        next.push((player, keep));
    }
    let mut contexts = world.resource_mut::<InputContexts>();
    for (player, keep) in next {
        if keep.is_empty() {
            contexts.remove_player(player);
        } else {
            contexts.set(player, keep);
        }
    }
    saved
}

/// Puts the saved stacks back, keeping contexts pushed during the pause.
fn restore_contexts(world: &mut World, saved: BTreeMap<u8, Vec<String>>) {
    let Some(mut contexts) = world.get_resource_mut::<InputContexts>() else {
        return;
    };
    for (player, mut stack) in saved {
        for context in contexts.enabled(player) {
            if !stack.contains(context) {
                stack.push(context.clone());
            }
        }
        contexts.set(player, stack);
    }
}

/// Resolves a string: the game's localization first, then the bundled table in the game's language
/// (or English).
fn tr(world: &mut World, key: &str, args: &[(String, String)]) -> String {
    if let Some(game) = world.get_resource::<Localization>()
        && game.has(key)
    {
        return game.resolve(key, args);
    }
    let locale = world
        .get_resource::<Localization>()
        .map(|l| l.locale().to_owned())
        .unwrap_or_default();
    let mut pause = world.resource_mut::<GamePause>();
    let strings = pause.strings.get_or_insert_with(|| {
        let sources = BUNDLED_FTL
            .iter()
            .map(|(l, s)| ((*l).to_owned(), (*s).to_owned()))
            .collect();
        Localization::new(sources, Some("en".to_owned())).unwrap_or_default()
    });
    let lang = locale.split(['-', '_']).next().unwrap_or("en");
    if !strings.set_locale(&locale) && strings.locale() != lang && !strings.set_locale(lang) {
        strings.set_locale("en");
    }
    strings.resolve(key, args)
}

/// The label of the bound pause input for the device the player last used (the keyboard key when
/// the pad has none); `None` when nothing is bound.
#[must_use]
pub fn pause_binding_label(world: &World) -> Option<String> {
    let config = world.get_resource::<InputConfig>()?;
    let device = world
        .get_resource::<regia_ui::hints::HintDevice>()
        .copied()
        .unwrap_or_default();
    let glyphs = world
        .get_resource::<regia_ui::input_glyphs::InputGlyphs>()
        .cloned()
        .unwrap_or_default();
    glyphs
        .lookup(TOGGLE_PAUSE_ACTION, device, Some(config))
        .or_else(|| {
            glyphs.lookup(
                TOGGLE_PAUSE_ACTION,
                regia_ui::hints::HintDevice::KeyboardMouse,
                Some(config),
            )
        })
        .map(|hit| hit.glyph)
}

/// The built-in screen's hint text.
fn hint_text(world: &mut World) -> String {
    match pause_binding_label(world) {
        Some(key) => tr(world, "pause-hint", &[("key".to_owned(), key)]),
        None => tr(world, "pause-hint-no-key", &[]),
    }
}

/// Spawns the configured screen (the built-in one when an asset is missing).
fn show_screen(world: &mut World) {
    let screen = world.resource::<GamePause>().config.screen.clone();
    world.resource_mut::<GamePause>().screen_error = None;
    let shown = match screen {
        PauseScreen::None => None,
        PauseScreen::Default => Some(build_default_screen(world)),
        PauseScreen::Asset(path) => {
            let name = ui_template_name(&path);
            match regia_ui::authoring::spawn_ui(world, &name) {
                Ok(root) => {
                    world.entity_mut(root).insert((
                        PauseScreenRoot,
                        InputLayer {
                            mode: LayerInputMode::UiOnly,
                        },
                    ));
                    Some(Shown {
                        root,
                        kind: "asset",
                        built_for: None,
                    })
                }
                Err(error) => {
                    tracing::warn!(target: "regia_framework", code = "pause_screen_missing", "pause screen {path:?}: {error}; showing the built-in screen");
                    world.resource_mut::<GamePause>().screen_error =
                        Some(format!("pause_screen_missing: {error}"));
                    Some(build_default_screen(world))
                }
            }
        }
        PauseScreen::Custom(builder) => {
            let root = (builder.0)(world);
            if let Ok(mut entity) = world.get_entity_mut(root) {
                entity.insert(PauseScreenRoot);
            }
            Some(Shown {
                root,
                kind: "custom",
                built_for: None,
            })
        }
    };
    world.resource_mut::<GamePause>().shown = shown;
    if let Some(mut dirty) = world.get_resource_mut::<regia_ui::LayoutDirty>() {
        dirty.mark();
    }
}

/// Despawns the screen showing.
fn hide_screen(world: &mut World) {
    let Some(shown) = world.resource_mut::<GamePause>().shown.take() else {
        return;
    };
    if let Ok(entity) = world.get_entity_mut(shown.root) {
        entity.despawn();
    }
    if let Some(mut dirty) = world.get_resource_mut::<regia_ui::LayoutDirty>() {
        dirty.mark();
    }
}

fn text(world: &mut World, s: String, px: f32, color: UiColor) -> Entity {
    world
        .spawn((
            UiNode,
            Text {
                spans: vec![TextSpan {
                    style: TextStyle {
                        font_px: px,
                        color,
                        ..Default::default()
                    },
                    ..TextSpan::lit(s)
                }],
                align: TextAlign::Center,
                wrap: WrapMode::Word,
                ..Default::default()
            },
        ))
        .id()
}

/// Builds the built-in screen (see the module docs): neutral engine styling, no branding.
fn build_default_screen(world: &mut World) -> Shown {
    let generation = world.get_resource::<Localization>().map_or(0, |l| l.generation.0);
    let title = tr(world, "pause-title", &[]);
    let description = tr(world, "pause-dialog-description", &[]);
    let hint = hint_text(world);
    let resume = tr(world, "pause-resume", &[]);
    let quit = tr(world, "pause-quit-to-menu", &[]);
    // A dimmed full-window backdrop on a UI-only layer: the frozen scene shows through.
    let root = world
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                sort_order: SCREEN_SORT_ORDER,
                ..Default::default()
            },
            UiNode,
            FocusScope,
            PauseScreenRoot,
            InputLayer {
                mode: LayerInputMode::UiOnly,
            },
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                align_items: Align::Center,
                justify_content: JustifyContent::Center,
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(UiColor::linear(0.0, 0.0, 0.0, 0.6)),
            },
            Accessible::default()
                .with_role(A11yRole::Dialog)
                .with_label(title.clone())
                .with_description(description),
            Name::new("pause-screen"),
        ))
        .id();
    let panel = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                align_items: Align::Center,
                padding: Edges::px(32.0),
                gap: [Dimension::Points(16.0), Dimension::Points(16.0)],
                ..Default::default()
            }),
            Background {
                kind: BackgroundKind::Color(UiColor::linear(0.02, 0.02, 0.025, 0.92)),
            },
        ))
        .id();
    let title_node = text(world, title, 40.0, UiColor::WHITE);
    world
        .entity_mut(title_node)
        .insert(Accessible::default().with_role(A11yRole::Heading));
    let hint_node = text(world, hint.clone(), 18.0, UiColor::linear(0.75, 0.75, 0.75, 1.0));
    world.entity_mut(hint_node).insert(PauseScreenHint);
    let buttons = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                align_items: Align::Center,
                gap: [Dimension::Points(10.0), Dimension::Points(10.0)],
                ..Default::default()
            }),
        ))
        .id();
    for (label, command, primary) in [
        (resume, RESUME_COMMAND, true),
        (quit, QUIT_TO_MENU_COMMAND, false),
    ] {
        let classes: &[&str] = if primary { &["primary"] } else { &[] };
        let mut commands = world.commands();
        let button = spawn_button(&mut commands, buttons, &label, classes);
        commands
            .entity(button)
            .insert((UiCommandName(command.to_owned()), Name::new(command.to_owned())));
        if primary {
            commands.entity(button).insert(DefaultFocus);
        }
        world.flush();
    }
    let mut panel_entity = world.entity_mut(panel);
    panel_entity.add_child(title_node);
    panel_entity.add_child(hint_node);
    panel_entity.add_child(buttons);
    world.entity_mut(root).add_child(panel);
    Shown {
        root,
        kind: "default",
        built_for: Some((generation, hint)),
    }
}

/// Adds the pause key (see the module docs). Read the project setting with
/// [`GamePauseConfig::load`], or pass `config`.
#[derive(Debug, Clone, Default)]
pub struct GamePausePlugin {
    /// The project root; `None` detects it (`$REGIA_PROJECT` or the nearest `Regia.toml`).
    pub project_root: Option<PathBuf>,
    /// Use these settings instead of reading the manifest.
    pub config: Option<GamePauseConfig>,
}

impl Plugin for GamePausePlugin {
    fn build(&self, app: &mut App) {
        // A `GamePause` inserted before the plugin (a game using `DefaultPlugins` that sets the pause
        // up in code) is kept; otherwise the settings come from `config` or the project.
        let existing = app.world().contains_resource::<GamePause>();
        let config = match &self.config {
            _ if existing => app.world().resource::<GamePause>().config.clone(),
            Some(config) => config.clone(),
            None => {
                let root = self
                    .project_root
                    .clone()
                    .unwrap_or_else(regia_asset::detect_project_root);
                let result = if regia_core::brand::project_manifest(&root).is_some() {
                    GamePauseConfig::load(&root)
                } else {
                    // A packaged game: `regia package` copied the table into `build_info.json`. An
                    // explicit root may name a staged package; otherwise the executable's report.
                    let local = self
                        .project_root
                        .as_ref()
                        .map(|root| crate::packaged_config::PackagedConfig::read_directory(root))
                        .filter(|package| package.location().is_some());
                    let package = match local {
                        Some(package) => package,
                        None => {
                            crate::packaged_config::ensure_loaded(app.world_mut());
                            app.world()
                                .resource::<crate::packaged_config::PackagedConfig>()
                                .clone()
                        }
                    };
                    GamePauseConfig::from_package(&package).map(Option::unwrap_or_default)
                };
                result.unwrap_or_else(|error| {
                    tracing::warn!(target: "regia_framework", code = "game_settings_invalid", "pause settings ignored: {error}");
                    GamePauseConfig::default()
                })
            }
        };
        if config.enabled {
            tracing::info!(target: "regia_framework", key = ?config.key, gamepad = ?config.gamepad, screen = %config.screen.setting(), "game pause on");
        }
        if !existing {
            app.insert_resource(GamePause::new(config));
        }
        app.init_resource::<NetworkedSession>()
            .add_message::<GamePaused>()
            .add_message::<GameResumed>()
            .add_message::<PauseRefused>()
            .add_message::<PauseScreenEvent>()
            .add_systems(Startup, add_pause_binding)
            .add_systems(
                Update,
                (
                    pause_screen_nav.before(regia_ui::focus::ui_focus_navigation),
                    game_pause_update
                        .after(regia_ui::widgets::ui_widgets)
                        .in_set(GamePauseSet),
                ),
            );
    }
}

/// `Startup`: the bindings, when the pause is on from the start.
fn add_pause_binding(world: &mut World) {
    ensure_bindings(world);
}

/// The default [`TOGGLE_PAUSE_ACTION`] bindings (also in the reset defaults), the player's saved
/// rebinding of them, and the pause context for player 0; once, the first time the pause is on.
fn ensure_bindings(world: &mut World) {
    let (enabled, added) = {
        let pause = world.resource::<GamePause>();
        (pause.config.enabled, pause.bindings_added)
    };
    if !enabled || added {
        return;
    }
    if !world.contains_resource::<InputConfig>() {
        // No input layer yet: try again on a later frame.
        return;
    }
    world.resource_mut::<GamePause>().bindings_added = true;
    let config = world.resource::<GamePause>().config.clone();
    let added = match world.get_resource_mut::<InputConfig>() {
        Some(mut input) => add_pause_bindings(&mut input, &config),
        None => false,
    };
    if let Some(mut defaults) = world.get_resource_mut::<InputDefaults>() {
        add_pause_bindings(&mut defaults.0, &config);
    }
    // `InputPlugin` merged the saved overrides before this binding existed: merge the pause
    // context's part again so a rebinding of the pause key survives a restart.
    if added && let Some(overrides) = world.get_resource::<InputOverrides>() {
        let mut ours = InputOverrides::default();
        if let Some(c) = overrides.contexts.get(PAUSE_CONTEXT) {
            ours.contexts.insert(PAUSE_CONTEXT.to_owned(), c.clone());
        }
        if let Some(s) = overrides.slots.get(PAUSE_CONTEXT) {
            ours.slots.insert(PAUSE_CONTEXT.to_owned(), s.clone());
        }
        if let Some(mut input) = world.get_resource_mut::<InputConfig>() {
            input.merge_overrides(&ours);
        }
    }
    if let Some(mut contexts) = world.get_resource_mut::<InputContexts>() {
        contexts.push(0, PAUSE_CONTEXT);
    }
}

/// `Update`, before focus navigation: the gamepad d-pad and South button drive the screen's focus
/// while it shows (keyboard and pointer reach the UI directly).
fn pause_screen_nav(pause: Res<GamePause>, pads: Option<Res<Gamepads>>, nav: Option<ResMut<UiNavActions>>) {
    if pause.shown.is_none() {
        return;
    }
    let (Some(pads), Some(mut nav)) = (pads, nav) else {
        return;
    };
    let pressed = |b| pads.just_pressed(b).is_some();
    nav.up |= pressed(GamepadButton::DPadUp);
    nav.down |= pressed(GamepadButton::DPadDown);
    nav.submit |= pressed(GamepadButton::South);
}

/// Whether [`TOGGLE_PAUSE_ACTION`] was pressed this frame by any player. In an app without the
/// action layer (no `InputConfig`: a game that reads raw devices) the configured key and gamepad
/// button are read directly instead (no rebinding there).
fn toggle_pressed(world: &mut World) -> bool {
    if world.contains_resource::<InputConfig>() {
        // The frame-level edge, or the action's value going from zero to non-zero. The level edge
        // matters after a pause: the zero-step frames of a pause keep the fixed-step edge state the
        // last step left (the press that paused), so the first press after a resume by a screen
        // button would otherwise read as held, not pressed.
        let (edge, held) = world.get_resource::<Actions>().map_or((false, false), |actions| {
            let states = actions
                .players
                .values()
                .filter_map(|p| p.states.get(TOGGLE_PAUSE_ACTION));
            states.fold((false, false), |(edge, held), s| {
                (
                    edge || s.just_pressed_this_frame(),
                    held || s.value != regia_core::math::Vec3::ZERO,
                )
            })
        });
        let mut pause = world.resource_mut::<GamePause>();
        let rose = held && !pause.toggle_held;
        pause.toggle_held = held;
        return edge || rose;
    }
    // No allocation per frame: the cursor reads in place and the key is compared by reference.
    let key_pressed = world.resource_scope(|world, mut pause: Mut<GamePause>| {
        let Some(messages) = world.get_resource::<Messages<KeyboardInput>>() else {
            return false;
        };
        let GamePause {
            config, key_cursor, ..
        } = &mut *pause;
        let key = config.key.as_deref();
        key_cursor
            .read(messages)
            .filter(|e| e.state == ButtonState::Pressed && !e.repeat && Some(e.physical_key.as_str()) == key)
            .count()
            > 0
    });
    let gamepad = world.resource::<GamePause>().config.gamepad;
    let pad_pressed = gamepad.is_some_and(|button| {
        world
            .get_resource::<Gamepads>()
            .is_some_and(|pads| pads.just_pressed(button).is_some())
    });
    key_pressed || pad_pressed
}

/// `Update`, after the widgets: the pause key, code requests and screen commands; keeps the pause
/// context, the released cursor and the built-in screen's strings current.
fn game_pause_update(world: &mut World) {
    let _span = tracing::trace_span!("game_pause").entered();
    let commands: Vec<UiCommand> = world.resource_scope(|world, mut pause: Mut<GamePause>| {
        world
            .get_resource::<Messages<UiCommand>>()
            .map(|m| pause.command_cursor.read(m).cloned().collect())
            .unwrap_or_default()
    });
    ensure_bindings(world);
    let enabled = world.resource::<GamePause>().config.enabled;
    if enabled
        && let Some(mut contexts) = world.get_resource_mut::<InputContexts>()
        && !contexts.is_enabled(0, PAUSE_CONTEXT)
    {
        contexts.push(0, PAUSE_CONTEXT);
    }
    if enabled && toggle_pressed(world) {
        world
            .resource_mut::<GamePause>()
            .requests
            .push(Request::Toggle(PauseSource::Input));
    }
    let paused = world.resource::<GamePause>().paused;
    for command in commands {
        let event = match command.name.as_str() {
            RESUME_COMMAND => PauseScreenEvent::Resume,
            QUIT_TO_MENU_COMMAND => PauseScreenEvent::QuitToMenu,
            _ => continue,
        };
        if !paused {
            continue;
        }
        world.write_message(event);
        resume_now(world, PauseSource::Screen);
    }
    let requests = std::mem::take(&mut world.resource_mut::<GamePause>().requests);
    for request in requests {
        let paused = world.resource::<GamePause>().paused;
        match request {
            Request::Pause(source) | Request::Toggle(source) if !paused => {
                let _ = pause_now(world, source);
            }
            Request::Resume(source) | Request::Toggle(source) if paused => {
                resume_now(world, source);
            }
            _ => {}
        }
    }
    let (paused, cursor_ours) = {
        let pause = world.resource::<GamePause>();
        (pause.paused, pause.cursor_ours)
    };
    if !paused {
        return;
    }
    // Keep the cursor free while paused (an overlay closing may have cleared the release).
    if cursor_ours
        && let Some(mut release) = world.get_resource_mut::<CursorRelease>()
        && !release.0
    {
        release.0 = true;
    }
    // A screen swapped while paused; the built-in screen after a language switch or a rebinding.
    let (swap, built_for) = {
        let pause = world.resource::<GamePause>();
        let want = pause.config.screen.kind();
        let swap = match &pause.shown {
            None => want != "none" && pause.screen_error.is_none(),
            Some(shown) => shown.kind != want && !(shown.kind == "default" && pause.screen_error.is_some()),
        };
        (swap, pause.shown.as_ref().and_then(|s| s.built_for.clone()))
    };
    let stale = match built_for {
        Some((generation, hint)) => {
            let now = world.get_resource::<Localization>().map_or(0, |l| l.generation.0);
            now != generation || hint_text(world) != hint
        }
        None => false,
    };
    if swap || stale {
        hide_screen(world);
        show_screen(world);
    }
}

#[cfg(test)]
mod tests;
